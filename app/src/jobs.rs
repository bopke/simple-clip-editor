//! Worker → UI messaging and the background frame fetcher.
//!
//! The UI thread never blocks. Every long operation runs on a worker, reports
//! progress over a channel, and can be cancelled by killing its child process.

use crate::audio::{Pcm, Waveform};
use crate::ffmpeg::Cancel;
use clip_core::{FrameIdx, RationalTime};
use crossbeam_channel::{Receiver, Sender};
use std::path::PathBuf;
use std::sync::Arc;

/// A decoded preview frame. `generation` lets the UI drop frames belonging to
/// a superseded request without tracking requests itself.
#[derive(Clone)]
pub struct DecodedFrame {
    pub generation: u64,
    pub frame: FrameIdx,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

pub enum Msg {
    Status(String),
    Error(String),

    BootstrapProgress {
        done: u64,
        total: u64,
    },
    BootstrapDone(Result<crate::ffmpeg::Tools, String>),

    ProxyProgress(f32),
    ProxyReady(PathBuf),
    PcmReady(Arc<Pcm>),
    WaveformReady(Arc<Waveform>),
    ThumbsReady(Vec<PathBuf>),
    ImportFailed(String),

    TrackReady {
        index: usize,
        duration: Option<RationalTime>,
        pcm: Option<Arc<Pcm>>,
    },

    Frame(DecodedFrame),
    FrameFailed(String),

    ExportProgress {
        fraction: f32,
        label: String,
    },
    ExportDone(Result<PathBuf, String>),
}

pub type Tx = Sender<Msg>;
pub type Rx = Receiver<Msg>;

pub fn channel() -> (Tx, Rx) {
    crossbeam_channel::unbounded()
}

/// A running background job the UI can cancel.
pub struct Job {
    pub cancel: Cancel,
    pub label: String,
}

impl Job {
    pub fn new(label: impl Into<String>) -> Job {
        Job {
            cancel: Cancel::new(),
            label: label.into(),
        }
    }
}

/// How many segments encode at once. Each x264 instance is already
/// multithreaded, so a small cap beats one per core.
pub fn export_parallelism() -> usize {
    num_cpus::get().clamp(1, 8).div_ceil(2).clamp(1, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallelism_is_capped() {
        let n = export_parallelism();
        assert!((1..=4).contains(&n), "{n}");
    }
}
