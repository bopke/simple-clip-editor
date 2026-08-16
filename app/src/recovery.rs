//! Crash recovery: an unsaved edit survives the process dying.
//!
//! While there are unsaved changes the app writes the same `.clip` bytes it
//! would save, to a fixed path in the cache directory, at most every
//! [`INTERVAL`], and once more on the way out. Saving removes it. So a snapshot
//! found at startup means exactly one thing: the previous run ended with work
//! that was never saved anywhere — whether it crashed or was simply closed.
//!
//! One slot, deliberately. It makes "was there a crash" a file-exists check
//! rather than a scan with heuristics about which snapshot belongs to whom.
//! The cost is that two copies of the app running at once share the slot — the
//! second to autosave wins, and only its work is offered back.

use crate::project_file::{self, ClipFile, ViewState};
use anyhow::Result;
use clip_core::Project;
use std::path::PathBuf;
use std::time::Duration;

/// How often an unsaved edit is written out. Long enough not to churn the disk
/// while dragging a crop rectangle, short enough that a crash costs less work
/// than it takes to be annoyed about.
pub const INTERVAL: Duration = Duration::from_secs(15);

/// Where the snapshot lives. `None` when there is no cache directory to put it
/// in, which is the same condition under which the app has no proxy cache
/// either — recovery is simply off.
pub fn path() -> Option<PathBuf> {
    crate::cache::cache_root()
        .ok()
        .map(|root| root.join("recovery").join("autosave.clip"))
}

/// Unsaved work found at startup, waiting to be taken or left.
pub struct Pending {
    pub file: ClipFile,
}

impl Pending {
    /// What the user recognises it by: the video they were editing.
    pub fn label(&self) -> String {
        self.file
            .project
            .source
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "an unsaved edit".to_string())
    }
}

/// The snapshot left by a previous run, if there is one.
///
/// An unreadable or half-written snapshot is discarded rather than reported:
/// there is nothing the user can do about it, and a startup error box about a
/// file they never knew existed is worse than silently having nothing to
/// restore.
pub fn pending() -> Option<Pending> {
    let path = path()?;
    match project_file::load(&path) {
        Ok(file) => Some(Pending { file }),
        Err(_) => {
            discard();
            None
        }
    }
}

pub fn write(project: &Project, view: &ViewState) -> Result<()> {
    let Some(path) = path() else { return Ok(()) };
    let bytes = project_file::to_bytes(project, view)?;
    project_file::write_atomically(&path, &bytes)
}

pub fn discard() {
    if let Some(path) = path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clip_core::{FrameIdx, Rational, RationalTime, Segment, SourceInfo};
    use std::sync::{Mutex, MutexGuard};

    /// There is one snapshot path, and these tests write to it. Taking it in
    /// turns keeps them from reading each other's writes — and restoring what
    /// was there keeps them from eating a real unsaved edit belonging to
    /// whoever is running the suite.
    static SLOT: Mutex<()> = Mutex::new(());

    struct Slot<'a> {
        _guard: MutexGuard<'a, ()>,
        existing: Option<Vec<u8>>,
    }

    impl Slot<'_> {
        fn take() -> Slot<'static> {
            let guard = SLOT.lock().unwrap_or_else(|e| e.into_inner());
            let existing = path().and_then(|p| std::fs::read(p).ok());
            discard();
            Slot {
                _guard: guard,
                existing,
            }
        }
    }

    impl Drop for Slot<'_> {
        fn drop(&mut self) {
            match (&self.existing, path()) {
                (Some(bytes), Some(p)) => {
                    let _ = std::fs::write(p, bytes);
                }
                _ => discard(),
            }
        }
    }

    fn project() -> Project {
        let fps = Rational::from_int(30);
        let mut p = Project::new(SourceInfo {
            path: "/media/capture.mkv".into(),
            duration: RationalTime::new(60, 1),
            avg_frame_rate: fps,
            r_frame_rate: fps,
            ..Default::default()
        });
        p.segments = vec![Segment::from_frames(FrameIdx(11), FrameIdx(97), fps)];
        p
    }

    #[test]
    fn the_snapshot_lives_under_the_cache_root() {
        let Some(p) = path() else { return };
        assert!(p.ends_with("recovery/autosave.clip"));
        assert!(p.starts_with(crate::cache::cache_root().unwrap()));
    }

    /// The whole point: work that was never saved is still there next time.
    #[test]
    fn unsaved_work_survives_a_process_that_never_came_back() {
        let _slot = Slot::take();
        assert!(pending().is_none(), "a clean start has nothing pending");

        let view = ViewState {
            playhead: FrameIdx(51),
            selection: Some((FrameIdx(10), FrameIdx(20))),
        };
        write(&project(), &view).unwrap();

        // As if the process had died here, and a new one started up.
        let found = pending().expect("unsaved work is offered back");
        assert_eq!(found.file.view, view);
        assert_eq!(
            found.file.project.segments[0].frame_span(Rational::from_int(30)),
            (FrameIdx(11), FrameIdx(97))
        );
        assert_eq!(found.label(), "capture.mkv");
    }

    /// Closing the window without saving is not meaningfully different from
    /// crashing, and must not be the one way to lose an edit for good.
    #[test]
    fn shutting_down_with_unsaved_work_keeps_it() {
        let _slot = Slot::take();
        let ctx = egui::Context::default();
        let mut app = crate::app::App::with_context(&ctx);
        app.project = Some(project());
        app.unsaved = true;

        app.finish_session();
        assert!(pending().is_some(), "unsaved work must survive the exit");

        // Saved work leaves nothing behind, so the next launch is quiet.
        app.unsaved = false;
        app.finish_session();
        assert!(pending().is_none());
    }

    /// Saving is what makes the next launch quiet.
    #[test]
    fn discarding_leaves_nothing_to_offer() {
        let _slot = Slot::take();
        write(&project(), &ViewState::default()).unwrap();
        assert!(pending().is_some());
        discard();
        assert!(pending().is_none());
        // Discarding twice is not an error; a clean exit with no snapshot is
        // the common case.
        discard();
    }

    /// Whatever is at the path, `pending` either returns something usable or
    /// nothing — it never propagates a parse error to startup.
    #[test]
    fn a_corrupt_snapshot_reads_as_nothing_to_restore() {
        let _slot = Slot::take();
        let p = match path() {
            Some(p) => p,
            None => return,
        };
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"{ this is not a project").unwrap();

        assert!(pending().is_none());
        // And it cleaned up after itself, so it is not asked about again.
        assert!(!p.exists());
    }
}
