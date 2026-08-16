//! Import pipeline workers: probe, proxy, PCM + waveform, thumbnail strip,
//! and the on-demand frame fetcher that scrubbing runs on.

use crate::audio::{Pcm, Waveform};
use crate::cache::SourceCache;
use crate::ffmpeg::{Cancel, Tools};
use crate::jobs::{DecodedFrame, Msg, Tx};
use clip_core::command;
use clip_core::progress::fraction;
use clip_core::{FrameIdx, Project, RationalTime, SourceInfo};
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

pub fn probe_duration(tools: &Tools, path: &Path) -> Option<RationalTime> {
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
            let waveform = if cache.has(&peaks_path) {
                Waveform::load(&peaks_path).ok()
            } else {
                match Waveform::compute(&pcm_path) {
                    Ok(w) => {
                        let _ = w.save(&peaks_path);
                        Some(w)
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Error(format!("waveform failed: {e}")));
                        None
                    }
                }
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
    if cache.thumbs().is_empty() {
        let _ = tx.send(Msg::Status("Rendering thumbnails…".into()));
        let args = command::thumbnail_args(&project, &cache.thumb_pattern());
        let _ = tools.run_with_progress(&args, &cancel, |_| {});
    }
    if !cancel.is_cancelled() {
        let _ = tx.send(Msg::ThumbsReady(cache.thumbs()));
        let _ = tx.send(Msg::Status(String::new()));
    }
}

/// Probe an added music track and decode it for the preview mixer.
pub fn load_track(tools: Tools, index: usize, path: PathBuf, dest: PathBuf, tx: Tx) {
    let duration = tools
        .probe(&command::ffprobe_args(&path))
        .ok()
        .and_then(|j| clip_core::probe::parse_duration(&j));
    let cancel = Cancel::new();
    let pcm = if tools
        .run_with_progress(&command::track_pcm_args(&path, &dest), &cancel, |_| {})
        .is_ok()
    {
        Pcm::load(&dest).ok()
    } else {
        None
    };
    let _ = tx.send(Msg::TrackReady {
        index,
        duration,
        pcm,
    });
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
        std::thread::Builder::new()
            .name("frame-server".into())
            .spawn(move || frame_loop(tools, rx, out))
            .expect("spawning frame server");
        FrameServer { tx, cancel }
    }

    pub fn request(&self, req: FrameRequest) {
        let _ = self.tx.send(req);
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

fn frame_loop(tools: Tools, rx: Receiver<FrameRequest>, out: Tx) {
    let cancel = Cancel::new();
    while let Ok(mut req) = rx.recv() {
        // Coalesce: only the newest position matters.
        while let Ok(newer) = rx.try_recv() {
            req = newer;
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
            Err(e) => {
                let _ = out.send(Msg::FrameFailed(e.to_string()));
            }
        }
    }
}
