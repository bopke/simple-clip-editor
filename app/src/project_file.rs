//! Reading and writing `.clip` project files.
//!
//! The file is the edit, not the media: it holds segment boundaries, crop,
//! audio settings and the paths of everything it refers to. Reopening it
//! re-imports the source, which rebuilds the proxy, waveform and thumbnails
//! from the cache if they are still there and from ffmpeg if they are not.
//!
//! JSON, pretty-printed. The model is small, saving is not on any hot path,
//! and a project file that a human can read and diff is worth more here than
//! a few saved bytes.

use anyhow::{bail, Context, Result};
use clip_core::{FrameIdx, Project};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const EXTENSION: &str = "clip";

/// Bumped when a change to the format cannot be read by an older build.
///
/// Adding a field with `#[serde(default)]` does not need a bump: an old file
/// loads with the default, and an old build ignores what it does not know.
/// Renaming, removing, or changing the meaning of a field does.
pub const FORMAT_VERSION: u32 = 1;

/// Where the playhead and selection were. Not part of the edit — reopening a
/// project where you left it is a convenience, and a file missing this section
/// still loads.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewState {
    #[serde(default)]
    pub playhead: FrameIdx,
    #[serde(default)]
    pub selection: Option<(FrameIdx, FrameIdx)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipFile {
    pub version: u32,
    /// Which build wrote this. Diagnostic only — nothing branches on it, since
    /// `version` is what describes the format.
    #[serde(default)]
    pub written_by: String,
    pub project: Project,
    #[serde(default)]
    pub view: ViewState,
}

impl ClipFile {
    pub fn new(project: Project, view: ViewState) -> ClipFile {
        ClipFile {
            version: FORMAT_VERSION,
            written_by: crate::version::COMMIT.to_string(),
            project,
            view,
        }
    }
}

/// Give `path` the `.clip` extension unless it already has one.
///
/// Only when there is no extension at all: a deliberate `edit.json` is left
/// alone, while the common case of typing a bare name in the save dialog gets
/// a file that can be double-clicked.
pub fn with_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_none() {
        path.with_extension(EXTENSION)
    } else {
        path
    }
}

pub fn is_project_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION))
}

/// Serialise to bytes. Separate from [`save`] so a recovery snapshot can be
/// written somewhere else without going through a file dialog.
pub fn to_bytes(project: &Project, view: &ViewState) -> Result<Vec<u8>> {
    let file = ClipFile::new(project.clone(), view.clone());
    Ok(serde_json::to_vec_pretty(&file)?)
}

pub fn save(path: &Path, project: &Project, view: &ViewState) -> Result<()> {
    let bytes = to_bytes(project, view)?;
    write_atomically(path, &bytes).with_context(|| format!("saving {}", path.display()))
}

/// Write via a temporary file in the same directory, flush it, then rename.
///
/// A crash — or a full disk — partway through writing must not leave a
/// truncated project where a good one used to be. Rename within a directory is
/// atomic on every platform this runs on.
///
/// Atomic is not the same as durable, and the recovery snapshot needs both. The
/// rename orders the *entry*, not the bytes: without `sync_all` on the tmp
/// file, a power loss just after the rename can leave the name pointing at
/// blocks that were never written, on ext4 and friends. Syncing the directory
/// afterwards is what makes the rename itself survive the same event. Both
/// together cost one fsync pair every 15 seconds while an edit is unsaved,
/// which is the trade a file whose entire job is surviving a hard stop should
/// be making.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{EXTENSION}.tmp"));
    let write = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    // Windows will not rename onto an existing file.
    #[cfg(windows)]
    let _ = std::fs::remove_file(path);
    match std::fs::rename(&tmp, path) {
        Ok(()) => {
            sync_dir(path.parent());
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

/// Flush the directory entry the rename just created.
///
/// Unix only, and best-effort: Windows has no directory handle to sync and
/// some filesystems refuse the open outright. A failure here means the write is
/// merely atomic rather than durable, which is where it was before — not a
/// reason to fail a save that has already landed.
fn sync_dir(dir: Option<&Path>) {
    #[cfg(unix)]
    if let Some(dir) = dir {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

pub fn load(path: &Path) -> Result<ClipFile> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    from_str(&text).with_context(|| format!("reading {}", path.display()))
}

pub fn from_str(text: &str) -> Result<ClipFile> {
    // The serde message is part of the error rather than a hidden cause: it
    // names the field and the line, and "not a Clip project file" on its own
    // tells the user nothing they can act on.
    let mut file: ClipFile =
        serde_json::from_str(text).map_err(|e| anyhow::anyhow!("not a Clip project file: {e}"))?;
    if file.version > FORMAT_VERSION {
        bail!(
            "this project was written by a newer version of Clip \
             (format {}, this build reads {FORMAT_VERSION})",
            file.version
        );
    }
    // Serde proved the shape; these prove the invariants. A file that passes
    // serde and fails the model is the dangerous case — it loads, and then
    // panics somewhere far away from here. What can be nudged back into range
    // is; what cannot is refused, because the alternative is a panic later.
    file.project.repair();
    file.project
        .validate()
        .map_err(|e| anyhow::anyhow!("this project file is not usable: {e}"))?;
    // Ids in the file were minted by whichever process wrote it, and this one
    // has its own counter and its own scratch files named from it.
    file.project.reassign_track_ids();
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clip_core::{
        AspectMode, AudioSourceSelection, AudioTrack, CropRect, Rational, RationalTime, Segment,
        SourceInfo,
    };

    fn nontrivial() -> Project {
        let fps = Rational::new(30000, 1001);
        let source = SourceInfo {
            path: "/media/capture.mkv".into(),
            duration: RationalTime::new(3607, 2),
            width: 2560,
            height: 1440,
            avg_frame_rate: fps,
            r_frame_rate: fps,
            rotation: 90,
            is_hdr: true,
            ..Default::default()
        };
        let mut p = Project::new(source);
        p.segments = vec![
            Segment::from_frames(FrameIdx(37), FrameIdx(451), fps),
            Segment::from_frames(FrameIdx(900), FrameIdx(1802), fps),
        ];
        p.crop = Some(CropRect {
            x: 100,
            y: 20,
            w: 608,
            h: 1080,
        });
        p.aspect_mode = AspectMode::Locked(Rational::new(9, 16));
        p.original_audio.source = AudioSourceSelection::MixAll;
        p.original_audio.gain_db = -4.5;
        let mut track = AudioTrack::new("/media/music.mp3".into());
        track.offset = RationalTime::new(-3, 2);
        track.trim_in = Some(RationalTime::new(11, 4));
        track.gain_db = -6.0;
        track.fade_out = true;
        track.duration = Some(RationalTime::new(190, 1));
        p.added_tracks = vec![track];
        p.output.long_edge = Some(1920);
        p.output.fps = Some(Rational::from_int(60));
        p
    }

    fn view() -> ViewState {
        ViewState {
            playhead: FrameIdx(1234),
            selection: Some((FrameIdx(100), FrameIdx(500))),
        }
    }

    #[test]
    fn a_project_round_trips() {
        let project = nontrivial();
        let text = String::from_utf8(to_bytes(&project, &view()).unwrap()).unwrap();
        let back = from_str(&text).unwrap();

        assert_eq!(back.version, FORMAT_VERSION);
        assert_eq!(back.view, view());
        // Compared field by field: `Project` has no `PartialEq`, and giving it
        // one only for a test would invite comparing projects in real code,
        // where two clones of an edit are not meaningfully "equal".
        let a = &back.project;
        assert_eq!(a.segments, project.segments);
        assert_eq!(a.project_fps, project.project_fps);
        assert_eq!(a.crop, project.crop);
        assert_eq!(a.aspect_mode, project.aspect_mode);
        assert_eq!(a.original_audio.source, project.original_audio.source);
        assert_eq!(a.original_audio.gain_db, project.original_audio.gain_db);
        assert_eq!(a.source.path, project.source.path);
        assert_eq!(a.source.duration, project.source.duration);
        assert_eq!(a.source.rotation, project.source.rotation);
        assert_eq!(a.source.is_hdr, project.source.is_hdr);
        assert_eq!(a.output.long_edge, project.output.long_edge);
        assert_eq!(a.output.fps, project.output.fps);
        assert_eq!(a.added_tracks.len(), 1);
        let (t, o) = (&a.added_tracks[0], &project.added_tracks[0]);
        assert_eq!(t.path, o.path);
        assert_eq!(t.offset, o.offset);
        assert_eq!(t.trim_in, o.trim_in);
        assert_eq!(t.gain_db, o.gain_db);
        assert_eq!(t.fade_out, o.fade_out);
        assert_eq!(t.duration, o.duration);
    }

    /// Exact times survive the trip as exact times, not as rounded decimals.
    #[test]
    fn ntsc_edit_points_are_not_rounded_by_the_file() {
        let project = nontrivial();
        let text = String::from_utf8(to_bytes(&project, &view()).unwrap()).unwrap();
        let back = from_str(&text).unwrap();
        let fps = Rational::new(30000, 1001);
        assert_eq!(
            back.project.segments[0].frame_span(fps),
            (FrameIdx(37), FrameIdx(451))
        );
    }

    #[test]
    fn track_ids_are_reassigned_on_load() {
        let project = nontrivial();
        let original = project.added_tracks[0].id;
        let text = String::from_utf8(to_bytes(&project, &view()).unwrap()).unwrap();
        let back = from_str(&text).unwrap();
        assert_ne!(back.project.added_tracks[0].id, original);
        // Two loads of the same file are two different projects, not one.
        let again = from_str(&text).unwrap();
        assert_ne!(
            again.project.added_tracks[0].id,
            back.project.added_tracks[0].id
        );
    }

    #[test]
    fn a_newer_format_is_refused_rather_than_misread() {
        let project = nontrivial();
        let text = String::from_utf8(to_bytes(&project, &view()).unwrap()).unwrap();
        let bumped = text.replacen(
            &format!("\"version\": {FORMAT_VERSION}"),
            &format!("\"version\": {}", FORMAT_VERSION + 1),
            1,
        );
        let err = from_str(&bumped).unwrap_err().to_string();
        assert!(err.contains("newer version"), "{err}");
    }

    /// A file written before the view section existed — or with only part of
    /// it — still opens; where the playhead was is not worth failing a load
    /// over.
    #[test]
    fn a_file_without_a_view_section_still_loads() {
        let project = nontrivial();
        let text = String::from_utf8(to_bytes(&project, &view()).unwrap()).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();

        value.as_object_mut().unwrap().remove("view");
        let back = from_str(&value.to_string()).expect("a missing view is not an error");
        assert_eq!(back.view, ViewState::default());

        value["view"] = serde_json::json!({});
        let back = from_str(&value.to_string()).expect("an empty view is not an error");
        assert_eq!(back.view, ViewState::default());
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(from_str("not json at all").is_err());
        assert!(from_str("{}").is_err());
    }

    /// The dangerous input is not the one serde rejects — it is the one serde
    /// *accepts* and the model cannot use. Each of these used to load happily
    /// and panic later, somewhere with no idea a file was involved.
    #[test]
    fn a_file_that_parses_but_breaks_the_model_is_refused() {
        let text = String::from_utf8(to_bytes(&nontrivial(), &view()).unwrap()).unwrap();
        let mutated = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
            f(&mut v);
            from_str(&v.to_string())
        };

        // `0/0` divides by zero in FrameIdx::from_time_round.
        let err = mutated(&|v| v["project"]["project_fps"] = serde_json::json!({"num":0,"den":0}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("zero denominator"), "{err}");

        // A rate that is a well-formed rational but still not a rate.
        let err = mutated(&|v| v["project"]["project_fps"] = serde_json::json!({"num":0,"den":1}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a rate"), "{err}");

        // A negative duration makes every frame count negative.
        let err = mutated(&|v| {
            v["project"]["source"]["duration"] = serde_json::json!({"num":-5,"den":1})
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("duration"), "{err}");

        // Unchanged, the same file still loads — the check is not just "no".
        assert!(mutated(&|_| {}).is_ok());
    }

    /// A crop over the edge of the picture is a bad `crop` filter argument, not
    /// a reason to withhold someone's entire edit. It comes back inside; the
    /// file still opens.
    #[test]
    fn an_out_of_bounds_crop_is_pulled_back_rather_than_refused() {
        let text = String::from_utf8(to_bytes(&nontrivial(), &view()).unwrap()).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
        v["project"]["crop"] = serde_json::json!({"x": 100, "y": 20, "w": 99999, "h": 99999});
        let back = from_str(&v.to_string()).expect("the edit is worth more than the crop");

        // The source is 2560x1440 rotated 90°, so the picture is 1440x2560.
        let crop = back.project.crop.unwrap();
        assert_eq!((crop.x, crop.y, crop.w, crop.h), (100, 20, 1340, 2540));
        back.project.validate().expect("repair leaves it valid");
    }

    /// The recovery snapshot goes through `from_str` too, and a half-flushed
    /// autosave is exactly the input that is still valid JSON.
    #[test]
    fn a_truncated_snapshot_is_an_error_not_a_panic() {
        let text = String::from_utf8(to_bytes(&nontrivial(), &view()).unwrap()).unwrap();
        for cut in [text.len() / 4, text.len() / 2, text.len() - 1] {
            assert!(from_str(&text[..cut]).is_err(), "truncated at {cut}");
        }
    }

    #[test]
    fn extensions() {
        assert_eq!(
            with_extension(PathBuf::from("/p/edit")),
            PathBuf::from("/p/edit.clip")
        );
        assert_eq!(
            with_extension(PathBuf::from("/p/edit.clip")),
            PathBuf::from("/p/edit.clip")
        );
        // An explicit extension is the user's choice, not a mistake to correct.
        assert_eq!(
            with_extension(PathBuf::from("/p/edit.json")),
            PathBuf::from("/p/edit.json")
        );
        assert!(is_project_file(Path::new("/p/a.clip")));
        assert!(is_project_file(Path::new("/p/a.CLIP")));
        assert!(!is_project_file(Path::new("/p/a.mkv")));
    }

    #[test]
    fn writing_is_atomic_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join("clip-project-file-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("edit.clip");
        let project = nontrivial();

        save(&path, &project, &view()).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("clip.tmp").exists());

        // Saving over an existing file replaces it rather than failing.
        save(&path, &project, &view()).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.project.segments, project.segments);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bytes have to be on the disk before the rename names them, or a
    /// power loss can leave a valid directory entry pointing at nothing. This
    /// cannot test the power loss, so it tests the property that makes it
    /// survivable: the file is complete and readable the instant `save`
    /// returns, with no temp file still in flight.
    #[test]
    fn a_saved_file_is_complete_the_moment_the_write_returns() {
        let dir = std::env::temp_dir().join("clip-project-file-durable");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("edit.clip");

        // A parent directory that does not exist yet is the recovery snapshot's
        // situation on a first run.
        let project = nontrivial();
        save(&path, &project, &view()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes,
            to_bytes(&project, &view()).unwrap(),
            "the file on disk is the whole file"
        );
        assert!(from_str(&String::from_utf8(bytes).unwrap()).is_ok());
        assert!(!path.with_extension("clip.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
