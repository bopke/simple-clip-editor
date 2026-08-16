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
use clip_core::{Project, RationalTime};
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
    let total_frames: u64 = segments.iter().map(|s| s.len()).sum();

    // Per-segment progress, combined into one weighted bar.
    let progress: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(vec![0.0; segments.len()]));
    let report = {
        let progress = progress.clone();
        let tx = tx.clone();
        let weights: Vec<f32> = segments
            .iter()
            .map(|s| s.len() as f32 / total_frames.max(1) as f32)
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
                    let seg_dur = RationalTime::new(seg.len() as i64 * fps.den, fps.num);
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
    use clip_core::{OutputPreset, SourceInfo};

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
