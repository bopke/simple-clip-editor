//! Import pipeline workers: probe, proxy, PCM + waveform, thumbnail strip,
//! and the on-demand frame fetcher that scrubbing runs on.

use crate::audio::{Pcm, Waveform};
use crate::cache::SourceCache;
use crate::ffmpeg::{Cancel, Tools};
use crate::jobs::{DecodedFrame, Msg, Tx};
use clip_core::command;
use clip_core::progress::fraction;
use clip_core::{FrameIdx, Project, RationalTime, SourceInfo, TrackId};
use crossbeam_channel::{Receiver, Sender};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// ffprobe a source file. Runs on a worker: ffprobe on a network drive is not
/// instant and the UI thread never blocks.
pub fn probe_source(tools: &Tools, path: &Path) -> Result<SourceInfo, String> {
    let json = tools
        .probe(&command::ffprobe_args(path))
        .map_err(|e| e.to_string())?;
    clip_core::probe::parse(&json, path).map_err(|e| e.to_string())
}

/// A file is only worth reading if it exists and is non-empty — a killed or
/// crashed ffmpeg leaves a zero-byte scratch file behind.
fn is_usable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// Duration only, for sources we do not otherwise inspect — an added music
/// track needs its length but nothing else ffprobe reports.
fn probe_duration(tools: &Tools, path: &Path) -> Option<RationalTime> {
    let json = tools.probe(&command::ffprobe_args(path)).ok()?;
    clip_core::probe::parse_duration(&json)
}

/// The whole import pipeline for one source, in the order the UI needs it:
/// proxy first (scrubbing), then audio (waveform), then thumbnails.
pub fn run_import(tools: Tools, project: Project, cache: SourceCache, tx: Tx, cancel: Cancel) {
    cache.touch();
    let has_audio = project.has_audio();

    // --- proxy ---------------------------------------------------------
    let proxy = cache.proxy();
    if !cache.has(&proxy) {
        let _ = tx.send(Msg::Status("Building scrub proxy…".into()));
        let args = command::proxy_args(&project, &proxy);
        let total = project.source.duration;
        let tx2 = tx.clone();
        let result = tools.run_with_progress(&args, &cancel, |u| {
            let _ = tx2.send(Msg::ProxyProgress(fraction(&u, total)));
        });
        if let Err(e) = result {
            let _ = std::fs::remove_file(&proxy);
            if !cancel.is_cancelled() {
                let _ = tx.send(Msg::ImportFailed(format!("proxy generation failed: {e}")));
            }
            return;
        }
    }
    let _ = tx.send(Msg::ProxyProgress(1.0));
    let _ = tx.send(Msg::ProxyReady(proxy));
    if cancel.is_cancelled() {
        return;
    }

    // --- audio: PCM for playback, peaks for the waveform ----------------
    if has_audio {
        let pcm_path = cache.pcm();
        if !cache.has(&pcm_path) {
            let _ = tx.send(Msg::Status("Extracting audio…".into()));
            let args = command::audio_extract_args(&project, &pcm_path);
            if let Err(e) = tools.run_with_progress(&args, &cancel, |_| {}) {
                let _ = std::fs::remove_file(&pcm_path);
                if !cancel.is_cancelled() {
                    let _ = tx.send(Msg::Error(format!("audio extraction failed: {e}")));
                }
            }
        }
        if cancel.is_cancelled() {
            return;
        }
        if cache.has(&pcm_path) {
            let peaks_path = cache.peaks();
            // A cached peaks file that will not load is a damaged one, not a
            // reason to go without a waveform: the PCM it was derived from is
            // right here, so rebuild rather than remember the damage forever.
            let cached = cache
                .has(&peaks_path)
                .then(|| Waveform::load(&peaks_path).ok())
                .flatten();
            let waveform = match cached {
                Some(w) => Some(w),
                None => match Waveform::compute(&pcm_path) {
                    Ok(w) => {
                        let _ = w.save(&peaks_path);
                        Some(w)
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Error(format!("waveform failed: {e}")));
                        None
                    }
                },
            };
            if let Some(w) = waveform {
                let _ = tx.send(Msg::WaveformReady(Arc::new(w)));
            }
            match Pcm::load(&pcm_path) {
                Ok(pcm) => {
                    let _ = tx.send(Msg::PcmReady(pcm));
                }
                Err(e) => {
                    // Playback is optional; trimming still works without it.
                    let _ = tx.send(Msg::Error(format!("playback audio unavailable: {e}")));
                }
            }
        }
    }
    if cancel.is_cancelled() {
        return;
    }

    // --- thumbnail strip ------------------------------------------------
    // A strip is regenerated unless it is marked complete: "there are files in
    // the directory" would accept the half-written strip a killed run leaves,
    // and then show it forever. What is on disk is still sent to the UI below —
    // a partial strip beats none for this session, it just is not trusted for
    // the next one.
    if !cache.thumbs_complete() {
        let _ = tx.send(Msg::Status("Rendering thumbnails…".into()));
        cache.begin_thumbs();
        let args = command::thumbnail_args(&project, &cache.thumb_pattern());
        let done = tools.run_with_progress(&args, &cancel, |_| {}).is_ok();
        if done && !cancel.is_cancelled() {
            cache.finish_thumbs();
        }
    }
    if !cancel.is_cancelled() {
        let _ = tx.send(Msg::ThumbsReady(cache.thumbs()));
        let _ = tx.send(Msg::Status(String::new()));
    }
}

/// Probe an added music track and decode it for the preview mixer.
///
/// Carries the track's id, not its position: by the time this finishes the
/// track may have moved or been removed entirely.
///
/// A scratch file already at `dest` is read instead of decoding again. That is
/// what makes undoing a track removal cheap: the samples were dropped from
/// memory, not from disk. It reports no duration, because nothing was probed —
/// the caller keeps the duration it already has.
pub fn load_track(tools: Tools, id: TrackId, path: PathBuf, dest: PathBuf, tx: Tx) {
    if is_usable(&dest) {
        if let Ok(pcm) = Pcm::load(&dest) {
            let _ = tx.send(Msg::TrackReady {
                id,
                duration: None,
                pcm: Some(pcm),
            });
            return;
        }
    }
    let duration = probe_duration(&tools, &path);
    let cancel = Cancel::new();
    let pcm = if tools
        .run_with_progress(&command::track_pcm_args(&path, &dest), &cancel, |_| {})
        .is_ok()
    {
        Pcm::load(&dest).ok()
    } else {
        None
    };
    let _ = tx.send(Msg::TrackReady { id, duration, pcm });
}

// ---------------------------------------------------------------------------
// Frame fetching
// ---------------------------------------------------------------------------

pub struct FrameRequest {
    pub generation: u64,
    pub frame: FrameIdx,
    pub args: Vec<String>,
    /// Decoded height; width is derived from the byte count so the aspect of
    /// the proxy does not have to be tracked separately.
    pub height: u32,
}

/// Serialises frame decoding on one worker and always serves the newest
/// request — during a fast scrub, intermediate positions are worth nothing.
pub struct FrameServer {
    tx: Sender<FrameRequest>,
    cancel: Cancel,
}

impl FrameServer {
    pub fn spawn(tools: Tools, out: Tx) -> FrameServer {
        let (tx, rx) = crossbeam_channel::unbounded::<FrameRequest>();
        let cancel = Cancel::new();
        // The worker decodes under *this* token, not one of its own, which is
        // what makes `shutdown` able to reach the ffmpeg child it has running.
        let worker_cancel = cancel.clone();
        std::thread::Builder::new()
            .name("frame-server".into())
            .spawn(move || frame_loop(tools, rx, out, worker_cancel))
            .expect("spawning frame server");
        FrameServer { tx, cancel }
    }

    pub fn request(&self, req: FrameRequest) {
        let _ = self.tx.send(req);
    }

    /// Kill a decode in flight. Called on exit, where the thing worth stopping
    /// is the ffmpeg child: an orphan is not reliably cleaned up by the OS on
    /// any of the three platforms, and waiting for it to notice the closed
    /// stdout pipe is a race, not a guarantee.
    ///
    /// This does not join the worker. A cancelled token cannot wake a thread
    /// blocked in `recv()` — what ends `frame_loop` is the sender dropping
    /// when `App` does, moments later. Killing the child is synchronous here
    /// and is the part that matters.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

fn frame_loop(tools: Tools, rx: Receiver<FrameRequest>, out: Tx, cancel: Cancel) {
    while let Ok(mut req) = rx.recv() {
        // Coalesce: only the newest position matters.
        while let Ok(newer) = rx.try_recv() {
            req = newer;
        }
        // Once cancelled the token kills every child at spawn, so continuing
        // would just turn each queued request into a failure message aimed at
        // a UI that is going away.
        if cancel.is_cancelled() {
            break;
        }
        match tools.run_capture_stdout(&req.args, &cancel) {
            Ok(bytes) if !bytes.is_empty() => {
                let height = req.height.max(1);
                let width = (bytes.len() as u32 / (height * 4)).max(1);
                if width * height * 4 <= bytes.len() as u32 {
                    let _ = out.send(Msg::Frame(DecodedFrame {
                        generation: req.generation,
                        frame: req.frame,
                        width,
                        height,
                        rgba: Arc::new(bytes),
                    }));
                }
            }
            // A seek past the end simply produces nothing; that is not an error
            // worth putting in front of the user.
            Ok(_) => {}
            // A cancelled decode is a shutdown, not a failure to report.
            Err(_) if cancel.is_cancelled() => break,
            Err(e) => {
                let _ = out.send(Msg::FrameFailed(e.to_string()));
            }
        }
    }
}
