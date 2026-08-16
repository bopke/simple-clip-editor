//! Two-phase export.
//!
//! Phase A encodes each kept segment independently and in parallel, carrying
//! the original audio through as PCM. Phase B concatenates, mixes all audio
//! once, and encodes audio once — which is what keeps AAC priming clicks out
//! of the joins and lets added music span segment boundaries.

use crate::cache::ExportTemp;
use crate::ffmpeg::{Cancel, Tools};
use crate::jobs::Msg;
use anyhow::{bail, Result};
use clip_core::command::{self, HwEncoder};
use clip_core::progress::fraction;
use clip_core::Project;
use crossbeam_channel::Sender;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Phase B is a stream copy of the video plus one audio encode, so it is fast
/// regardless of clip length — a small fixed slice of the progress bar.
const PHASE_B_SHARE: f32 = 0.12;

pub fn run_export(
    tools: Tools,
    project: Project,
    hw: Option<HwEncoder>,
    out_path: PathBuf,
    tx: Sender<Msg>,
    cancel: Cancel,
) {
    match export(&tools, &project, hw, &out_path, &tx, &cancel) {
        Ok(()) => {
            let _ = tx.send(Msg::ExportProgress {
                fraction: 1.0,
                label: "Done".into(),
            });
            let _ = tx.send(Msg::ExportDone(Ok(out_path)));
        }
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            let msg = if cancel.is_user_cancelled() {
                "cancelled".to_string()
            } else {
                e.to_string()
            };
            let _ = tx.send(Msg::ExportDone(Err(msg)));
        }
    }
}

fn export(
    tools: &Tools,
    project: &Project,
    hw: Option<HwEncoder>,
    out_path: &Path,
    tx: &Sender<Msg>,
    cancel: &Cancel,
) -> Result<()> {
    let segments = project.normalised_segments();
    if segments.is_empty() {
        bail!("nothing to export — the timeline is empty");
    }
    let temp = ExportTemp::new()?;
    let fps = project.project_fps;
    let total_frames: u64 = segments.iter().map(|s| s.len_frames(fps)).sum();

    // Per-segment progress, combined into one weighted bar.
    let progress: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(vec![0.0; segments.len()]));
    let report = {
        let progress = progress.clone();
        let tx = tx.clone();
        let weights: Vec<f32> = segments
            .iter()
            .map(|s| s.len_frames(fps) as f32 / total_frames.max(1) as f32)
            .collect();
        move || {
            let p = progress.lock().expect("progress lock");
            let done: f32 = p.iter().zip(&weights).map(|(f, w)| f * w).sum();
            let _ = tx.send(Msg::ExportProgress {
                fraction: done * (1.0 - PHASE_B_SHARE),
                label: format!("Encoding {}%", (done * 100.0) as u32),
            });
        }
    };

    // --- Phase A: segments in parallel ---------------------------------
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<usize>();
    for i in 0..segments.len() {
        let _ = job_tx.send(i);
    }
    drop(job_tx);

    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let workers = crate::jobs::export_parallelism().min(segments.len());
    let segments = &segments;
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let job_rx = job_rx.clone();
            let progress = progress.clone();
            let errors = errors.clone();
            let report = report.clone();
            let temp_dir = &temp;
            scope.spawn(move || {
                while let Ok(i) = job_rx.recv() {
                    if cancel.is_cancelled() {
                        return;
                    }
                    let seg = segments[i];
                    let out = temp_dir.segment(i);
                    let args = command::export_segment_args(project, seg, hw, &out);
                    let seg_dur = seg.duration();
                    let result = tools.run_with_progress(&args, cancel, |u| {
                        if let Ok(mut p) = progress.lock() {
                            p[i] = fraction(&u, seg_dur);
                        }
                        report();
                    });
                    match result {
                        Ok(()) => {
                            if let Ok(mut p) = progress.lock() {
                                p[i] = 1.0;
                            }
                            report();
                        }
                        Err(e) => {
                            errors.lock().expect("error lock").push(e.to_string());
                            // One failed segment makes the whole export
                            // pointless; stop the others immediately.
                            cancel.abort();
                            return;
                        }
                    }
                }
            });
        }
    });

    if cancel.is_cancelled() {
        let errs = errors.lock().expect("error lock");
        if let Some(first) = errs.first() {
            bail!("{first}");
        }
        bail!("cancelled");
    }

    // --- Phase B: concat, mix, encode audio once ------------------------
    let paths: Vec<PathBuf> = (0..segments.len()).map(|i| temp.segment(i)).collect();
    std::fs::write(temp.list(), command::concat_list_contents(&paths))?;

    let _ = tx.send(Msg::ExportProgress {
        fraction: 1.0 - PHASE_B_SHARE,
        label: "Mixing audio…".into(),
    });
    let args = command::export_final_args(project, &temp.list(), out_path);
    let out_dur = project.output_duration();
    let tx2 = tx.clone();
    tools.run_with_progress(&args, cancel, |u| {
        let f = (1.0 - PHASE_B_SHARE) + fraction(&u, out_dur) * PHASE_B_SHARE;
        let _ = tx2.send(Msg::ExportProgress {
            fraction: f,
            label: "Mixing audio…".into(),
        });
    })?;

    // `temp` drops here, taking the segment intermediates with it.
    Ok(())
}

// ---------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------

/// What the user should be told before an export starts, if anything.
///
/// Both of these were silent. The output path defaults to `<source>_edit.mp4`
/// next to the source and Ctrl+E wrote it without comment, so a second export
/// after changing one setting overwrote the first with no way back. And Phase A
/// keeps every segment on disk at once — gigabytes for a long clip — so an
/// export could get twenty minutes in and die of a full disk.
#[derive(Debug, Default, PartialEq)]
pub struct Preflight {
    /// Set when the output file is already there. Carries its size, because
    /// "you are about to replace a 3 GB file" reads differently from "a 4 kB
    /// one".
    pub overwrite: Option<u64>,
    /// Volumes that look too small, with what is needed and what is there.
    pub tight: Vec<SpaceWarning>,
}

#[derive(Debug, PartialEq)]
pub struct SpaceWarning {
    /// What lands here: the finished file, or Phase A's intermediates.
    pub what: &'static str,
    pub path: PathBuf,
    pub needed: u64,
    pub available: u64,
}

impl Preflight {
    pub fn is_clear(&self) -> bool {
        self.overwrite.is_none() && self.tight.is_empty()
    }
}

/// Roughly how large Phase A's intermediates and the finished file will be.
///
/// Returns `(intermediates, output)`. Both are estimates and are described as
/// such wherever they are shown: the video half cannot be known without doing
/// the encode. What it *can* do is tell 8 GB from 800 MB, which is the
/// difference between an export that works and one that fills the disk twenty
/// minutes in.
///
/// The video estimate is the source's own size scaled by the fraction of it
/// being kept — the source is a real encode of the same footage, which beats
/// any bitrate table. The audio half is not a guess at all: Phase A carries
/// audio as `pcm_s16le` at the source's own rate and channel count, and that is
/// arithmetic. For an hour of stereo 48 kHz it is 690 MB, which is most of the
/// surprise.
pub fn estimated_bytes(project: &Project) -> (u64, u64) {
    let kept = project.output_duration().as_f64().max(0.0);
    let total = project.source.duration.as_f64();
    let source_bytes = std::fs::metadata(&project.source.path)
        .map(|m| m.len())
        .unwrap_or(0);
    let fraction = if total > 0.0 {
        (kept / total).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let video = (source_bytes as f64 * fraction) as u64;

    let streams: Vec<&clip_core::AudioStreamInfo> = match project.original_audio.source {
        clip_core::AudioSourceSelection::None => Vec::new(),
        clip_core::AudioSourceSelection::Stream(i) => {
            project.source.audio_streams.get(i).into_iter().collect()
        }
        clip_core::AudioSourceSelection::MixAll => project.source.audio_streams.iter().collect(),
    };
    let pcm_per_sec: u64 = if project.output.preset.has_audio() {
        streams
            .iter()
            .map(|s| u64::from(s.sample_rate) * u64::from(s.channels) * 2)
            .sum()
    } else {
        0
    };
    let intermediates = video.saturating_add((pcm_per_sec as f64 * kept) as u64);
    // The finished file re-encodes that audio to something small, so the output
    // is the video estimate plus a little.
    let output = video.saturating_add(video / 20);
    (intermediates, output)
}

/// Check an export before it starts. `scratch` is where Phase A will work.
pub fn preflight(project: &Project, out_path: &Path, scratch: Option<&Path>) -> Preflight {
    let (intermediates, output) = estimated_bytes(project);
    let mut tight = Vec::new();

    let mut check = |what: &'static str, path: &Path, needed: u64| {
        // `None` means the volume could not be measured — a network share, an
        // exotic filesystem. Silence is right there: a warning that fires
        // whenever the check fails is a warning nobody reads.
        if let Some(available) = crate::disk::available_bytes(path) {
            if needed > available {
                tight.push(SpaceWarning {
                    what,
                    path: path.to_path_buf(),
                    needed,
                    available,
                });
            }
        }
    };

    // Phase A's intermediates are still on disk while Phase B writes the
    // output, so on one volume the requirement is the sum and reporting them
    // separately would clear an export that cannot finish.
    match scratch {
        Some(scratch) if crate::disk::same_volume(out_path, scratch) == Some(true) => {
            check(
                "the exported file and its temporary files",
                out_path,
                output.saturating_add(intermediates),
            );
        }
        Some(scratch) => {
            check("the exported file", out_path, output);
            check("temporary files while encoding", scratch, intermediates);
        }
        None => check("the exported file", out_path, output),
    }

    Preflight {
        overwrite: std::fs::metadata(out_path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len()),
        tight,
    }
}

/// Default output path next to the source: `clip.mkv` → `clip_edit.mp4`.
pub fn suggested_output(project: &Project) -> PathBuf {
    let src = &project.source.path;
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "clip".into());
    let ext = project.output.preset.extension();
    src.with_file_name(format!("{stem}_edit.{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clip_core::{
        AudioStreamInfo, FrameIdx, OutputPreset, Rational, RationalTime, Segment, SourceInfo,
    };

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clip-editor-preflight-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A 100-second source of a known size, half of it kept, with one stereo
    /// 48 kHz stream.
    fn project_with_source(dir: &Path, source_bytes: usize) -> Project {
        let path = dir.join("capture.mkv");
        std::fs::write(&path, vec![0u8; source_bytes]).unwrap();
        let fps = Rational::from_int(30);
        let mut p = Project::new(SourceInfo {
            path,
            duration: RationalTime::new(100, 1),
            width: 1920,
            height: 1080,
            avg_frame_rate: fps,
            r_frame_rate: fps,
            audio_streams: vec![AudioStreamInfo {
                index: 0,
                codec: "aac".into(),
                channels: 2,
                sample_rate: 48_000,
                language: None,
                title: None,
            }],
            ..Default::default()
        });
        p.segments = vec![Segment::from_frames(FrameIdx(0), FrameIdx(1500), fps)];
        p
    }

    /// The audio half is arithmetic rather than a guess, and for anything long
    /// it is most of the surprise: Phase A carries audio as `pcm_s16le`, so a
    /// clip whose source is 1 MB still needs ten times that on disk.
    #[test]
    fn the_estimate_accounts_for_uncompressed_intermediate_audio() {
        let dir = dir("estimate");
        let mut project = project_with_source(&dir, 1_000_000);

        // Half of a 100 s source, so half its bytes.
        let (intermediates, output) = estimated_bytes(&project);
        assert_eq!(output, 500_000 + 25_000);
        // 48 kHz × 2 ch × 2 bytes × 50 s of PCM on top of the video.
        assert_eq!(intermediates, 500_000 + 9_600_000);

        // Muting the original takes the PCM out of Phase A entirely.
        project.original_audio.source = clip_core::AudioSourceSelection::None;
        assert_eq!(estimated_bytes(&project).0, 500_000);

        // So does a preset with no audio at all.
        project.original_audio.source = clip_core::AudioSourceSelection::Stream(0);
        project.output.preset = OutputPreset::Gif;
        assert_eq!(estimated_bytes(&project).0, 500_000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The output path defaults to `<source>_edit.mp4` and never passes through
    /// a save dialog, so nothing ever asked before replacing it.
    #[test]
    fn preflight_notices_a_file_it_is_about_to_replace() {
        let dir = dir("overwrite");
        let project = project_with_source(&dir, 1000);

        let fresh = dir.join("new.mp4");
        let checks = preflight(&project, &fresh, None);
        assert_eq!(checks.overwrite, None);
        assert!(
            checks.is_clear(),
            "a path with nothing at it needs no dialog"
        );

        std::fs::write(&fresh, vec![0u8; 4096]).unwrap();
        let checks = preflight(&project, &fresh, None);
        assert_eq!(
            checks.overwrite,
            Some(4096),
            "the size is part of the question"
        );
        assert!(!checks.is_clear());

        // A directory in the way is not a file about to be replaced; ffmpeg
        // will fail on it, and claiming otherwise would be its own wrong answer.
        let as_dir = dir.join("subdir");
        std::fs::create_dir_all(&as_dir).unwrap();
        assert_eq!(preflight(&project, &as_dir, None).overwrite, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real volume has room for a 1 MB estimate, so nothing should fire —
    /// a space warning the user cannot act on is one they learn to click past.
    #[test]
    fn a_normal_export_raises_nothing() {
        let dir = dir("clear");
        let project = project_with_source(&dir, 1_000_000);
        let checks = preflight(&project, &dir.join("out.mp4"), Some(&dir));
        assert!(checks.is_clear(), "{checks:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggested_name_follows_the_preset() {
        let mut p = Project::new(SourceInfo {
            path: PathBuf::from("/videos/raid boss.mkv"),
            ..Default::default()
        });
        assert_eq!(
            suggested_output(&p),
            PathBuf::from("/videos/raid boss_edit.mp4")
        );
        p.output.preset = OutputPreset::Gif;
        assert_eq!(
            suggested_output(&p),
            PathBuf::from("/videos/raid boss_edit.gif")
        );
    }
}
