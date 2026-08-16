//! Saving, reopening, and getting a removed track's audio back.
//!
//! These drive `App` directly rather than the UI, and none of them need
//! ffmpeg: reopening a project without tools restores the edit and simply does
//! not re-import behind it, and a track reload reads the scratch file rather
//! than decoding.

use clip_core::{AspectMode, CropRect, FrameIdx, Rational, RationalTime, Segment};
use clip_editor::app::{scratch_path, App};
use clip_editor::audio::Pcm;
use clip_editor::cache::SourceCache;
use clip_editor::ffmpeg::{Origin, Tools};
use clip_editor::project_file::{self, ViewState};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fps() -> Rational {
    Rational::from_int(30)
}

/// A directory of our own per test, so tests cannot see each other's files.
fn workdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("clipper-project-io-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A stand-in for the video the edit refers to. Never decoded — only its
/// existence and path matter here.
fn fake_source(dir: &Path) -> PathBuf {
    let path = dir.join("capture.mkv");
    std::fs::write(&path, b"not really a video").unwrap();
    path
}

fn app_with_project(ctx: &egui::Context, dir: &Path) -> App {
    let mut app = App::with_context(ctx);
    let source = clip_core::SourceInfo {
        path: fake_source(dir),
        width: 1920,
        height: 1080,
        duration: RationalTime::new(120, 1),
        avg_frame_rate: fps(),
        r_frame_rate: fps(),
        ..Default::default()
    };
    app.project = Some(clip_core::Project::new(source));
    // Enough of a toolchain to reach the code under test; the binaries are
    // never run, because every load here hits the scratch-file path.
    app.tools = Some(Tools {
        ffmpeg: dir.join("no-such-ffmpeg"),
        ffprobe: dir.join("no-such-ffprobe"),
        origin: Origin::Managed,
    });
    app.cache = Some(SourceCache {
        dir: dir.to_path_buf(),
    });
    app
}

fn tone(value: f32) -> Arc<Pcm> {
    Arc::new(Pcm {
        samples: vec![value; 64],
    })
}

/// Write PCM where a decode would have left it.
fn write_scratch(app: &App, id: clip_core::TrackId, value: f32) {
    let path = scratch_path(app.cache.as_ref().unwrap(), id);
    let bytes: Vec<u8> = vec![value; 64]
        .iter()
        .flat_map(|v: &f32| v.to_le_bytes())
        .collect();
    std::fs::write(path, bytes).unwrap();
}

/// Pump worker messages until `done` holds or the wait runs out. The loads
/// under test are real threads, so this is a real wait — a short one, since
/// they only read a file.
fn pump_until(app: &mut App, ctx: &egui::Context, done: impl Fn(&App) -> bool) -> bool {
    for _ in 0..200 {
        app.drain_messages(ctx);
        if done(app) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

// -- #4: save and reopen ----------------------------------------------------

#[test]
fn a_saved_project_reopens_as_the_same_edit() {
    let ctx = egui::Context::default();
    let dir = workdir("reopen");
    let mut app = app_with_project(&ctx, &dir);

    let segments = vec![
        Segment::from_frames(FrameIdx(37), FrameIdx(451), fps()),
        Segment::from_frames(FrameIdx(900), FrameIdx(1802), fps()),
    ];
    let crop = CropRect {
        x: 100,
        y: 20,
        w: 608,
        h: 1080,
    };
    {
        let p = app.project.as_mut().unwrap();
        p.segments = segments.clone();
        p.crop = Some(crop);
        p.aspect_mode = AspectMode::Locked(Rational::new(9, 16));
        p.original_audio.gain_db = -4.5;
        p.added_tracks = vec![clip_core::AudioTrack::new(dir.join("music.mp3"))];
    }
    app.playhead = FrameIdx(1234);
    app.selection = Some((FrameIdx(100), FrameIdx(500)));

    let path = dir.join("edit.clip");
    app.save_project_to(path.clone());
    assert_eq!(app.save_path.as_ref(), Some(&path));
    assert!(!app.unsaved, "saving clears the unsaved marker");

    // A fresh app, as if the editor had been closed and started again.
    let mut reopened = App::with_context(&ctx);
    reopened.open_project_path(path);

    let p = reopened.project.as_ref().expect("project is open");
    assert_eq!(p.segments, segments);
    assert_eq!(p.crop, Some(crop));
    assert_eq!(p.aspect_mode, AspectMode::Locked(Rational::new(9, 16)));
    assert_eq!(p.original_audio.gain_db, -4.5);
    assert_eq!(p.added_tracks.len(), 1);
    assert_eq!(p.added_tracks[0].path, dir.join("music.mp3"));
    assert_eq!(reopened.playhead, FrameIdx(1234));
    assert_eq!(reopened.selection, Some((FrameIdx(100), FrameIdx(500))));
    assert!(!reopened.unsaved);
    // Reopening is not an edit, so there is nothing to undo back past.
    assert!(!reopened.undo.can_undo());
}

#[test]
fn opening_a_project_by_path_dispatches_on_the_extension() {
    let ctx = egui::Context::default();
    let dir = workdir("dispatch");
    let mut app = app_with_project(&ctx, &dir);
    app.project.as_mut().unwrap().segments =
        vec![Segment::from_frames(FrameIdx(10), FrameIdx(20), fps())];
    let path = dir.join("edit.clip");
    app.save_project_to(path.clone());

    let mut other = App::with_context(&ctx);
    // `open_path`, not `open_project_path` — the generic entry point the file
    // dialog and drag-and-drop both use.
    other.open_path(path);
    assert_eq!(
        other.project.as_ref().unwrap().segments[0].frame_span(fps()),
        (FrameIdx(10), FrameIdx(20))
    );
}

#[test]
fn a_missing_source_is_reported_but_the_edit_still_opens() {
    let ctx = egui::Context::default();
    let dir = workdir("missing-source");
    let mut app = app_with_project(&ctx, &dir);
    let path = dir.join("edit.clip");
    app.save_project_to(path.clone());
    std::fs::remove_file(app.project.as_ref().unwrap().source.path.clone()).unwrap();

    let mut reopened = App::with_context(&ctx);
    reopened.open_project_path(path);

    assert!(reopened.project.is_some(), "the edit is still worth having");
    assert!(
        reopened.errors.iter().any(|e| e.contains("missing")),
        "{:?}",
        reopened.errors
    );
}

#[test]
fn a_project_written_by_a_newer_build_is_refused() {
    let ctx = egui::Context::default();
    let dir = workdir("newer");
    let mut app = app_with_project(&ctx, &dir);
    let path = dir.join("edit.clip");
    app.save_project_to(path.clone());

    let text = std::fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["version"] = serde_json::json!(project_file::FORMAT_VERSION + 1);
    std::fs::write(&path, value.to_string()).unwrap();

    let mut other = App::with_context(&ctx);
    other.open_project_path(path);
    assert!(other.project.is_none(), "nothing half-loaded");
    assert!(
        other.errors.iter().any(|e| e.contains("newer version")),
        "{:?}",
        other.errors
    );
}

#[test]
fn editing_marks_the_project_unsaved_and_saving_clears_it() {
    let ctx = egui::Context::default();
    let dir = workdir("dirty");
    let mut app = app_with_project(&ctx, &dir);
    assert!(!app.unsaved);

    app.playhead = FrameIdx(30);
    clip_editor::keys::set_in(&mut app);
    assert!(app.unsaved, "an edit is unsaved work");
    assert!(app.window_title().starts_with("• "));

    app.save_project_to(dir.join("edit.clip"));
    assert!(!app.unsaved);
    assert!(!app.window_title().starts_with("• "));
    assert!(app.window_title().contains("edit.clip"));
}

// -- #3: undo restores a removed track's audio ------------------------------

#[test]
fn undoing_a_track_removal_brings_its_audio_back() {
    let ctx = egui::Context::default();
    let dir = workdir("undo-audio");
    let mut app = app_with_project(&ctx, &dir);

    let track = clip_core::AudioTrack::new(dir.join("music.mp3"));
    let id = track.id;
    app.project.as_mut().unwrap().added_tracks.push(track);
    app.track_pcm.insert(id, tone(0.25));
    write_scratch(&app, id, 0.25);
    let scratch = scratch_path(app.cache.as_ref().unwrap(), id);

    app.remove_audio_track(0);
    assert!(app.project.as_ref().unwrap().added_tracks.is_empty());
    assert!(!app.track_pcm.contains_key(&id), "samples leave memory");
    assert!(scratch.exists(), "but not the disk — undo needs them");

    app.undo();
    assert_eq!(app.project.as_ref().unwrap().added_tracks.len(), 1);
    assert!(
        pump_until(&mut app, &ctx, |a| a.track_pcm.contains_key(&id)),
        "the restored track never got its audio back"
    );
    assert_eq!(app.track_pcm[&id].samples[0], 0.25);
    assert!(app.loading_tracks.is_empty(), "the load is accounted for");
}

/// The reload must not start a second decode for a track that is already
/// loading — two writers on one scratch file is how the file gets corrupted.
#[test]
fn a_track_already_loading_is_not_loaded_again() {
    let ctx = egui::Context::default();
    let dir = workdir("no-double-load");
    let mut app = app_with_project(&ctx, &dir);

    let track = clip_core::AudioTrack::new(dir.join("music.mp3"));
    let id = track.id;
    app.project.as_mut().unwrap().added_tracks.push(track);
    write_scratch(&app, id, 0.5);

    app.reload_missing_track_audio();
    assert!(app.loading_tracks.contains(&id));
    // A second ask while the first is in flight is a no-op.
    app.reload_missing_track_audio();
    assert_eq!(app.loading_tracks.len(), 1);

    assert!(pump_until(&mut app, &ctx, |a| a
        .track_pcm
        .contains_key(&id)));
    assert!(app.loading_tracks.is_empty());
}

/// A reload has no probe behind it, so it reports no duration — which must not
/// be mistaken for "this track has no duration".
#[test]
fn reloading_keeps_the_duration_the_project_already_had() {
    let ctx = egui::Context::default();
    let dir = workdir("keep-duration");
    let mut app = app_with_project(&ctx, &dir);

    let mut track = clip_core::AudioTrack::new(dir.join("music.mp3"));
    let id = track.id;
    track.duration = Some(RationalTime::new(190, 1));
    app.project.as_mut().unwrap().added_tracks.push(track);
    write_scratch(&app, id, 0.75);

    app.reload_missing_track_audio();
    assert!(pump_until(&mut app, &ctx, |a| a
        .track_pcm
        .contains_key(&id)));
    assert_eq!(
        app.project.as_ref().unwrap().added_tracks[0].duration,
        Some(RationalTime::new(190, 1))
    );
}

/// Reopening a saved project decodes its tracks again — the file holds paths,
/// not samples.
#[test]
fn reopening_a_project_reloads_its_track_audio() {
    let ctx = egui::Context::default();
    let dir = workdir("reopen-audio");
    let mut app = app_with_project(&ctx, &dir);
    app.project
        .as_mut()
        .unwrap()
        .added_tracks
        .push(clip_core::AudioTrack::new(dir.join("music.mp3")));
    let path = dir.join("edit.clip");
    app.save_project_to(path.clone());

    let mut reopened = app_with_project(&ctx, &dir);
    reopened.open_project_path(path);
    // Ids are minted fresh on load, so the scratch file is written where the
    // new id says — which is why this one has to decode rather than reuse.
    let id = reopened.project.as_ref().unwrap().added_tracks[0].id;
    assert!(
        reopened.loading_tracks.contains(&id),
        "opening a project must ask for its audio"
    );
}

// -- #5: crash recovery -----------------------------------------------------

#[test]
fn restoring_recovered_work_opens_it_as_unsaved() {
    let ctx = egui::Context::default();
    let dir = workdir("restore");
    let mut app = app_with_project(&ctx, &dir);
    app.project.as_mut().unwrap().segments =
        vec![Segment::from_frames(FrameIdx(5), FrameIdx(95), fps())];
    app.playhead = FrameIdx(42);

    // Exactly what the autosave writes, handed over the way startup finds it.
    let bytes = project_file::to_bytes(
        app.project.as_ref().unwrap(),
        &ViewState {
            playhead: app.playhead,
            selection: None,
        },
    )
    .unwrap();
    let file = project_file::from_str(&String::from_utf8(bytes).unwrap()).unwrap();

    let mut fresh = App::with_context(&ctx);
    fresh.pending_recovery = Some(clip_editor::recovery::Pending { file });
    fresh.restore_recovery();

    assert_eq!(
        fresh.project.as_ref().unwrap().segments[0].frame_span(fps()),
        (FrameIdx(5), FrameIdx(95))
    );
    assert_eq!(fresh.playhead, FrameIdx(42));
    // It was never saved anywhere, so it must not look saved — and must not
    // have inherited a path that Ctrl+S would overwrite.
    assert!(fresh.unsaved);
    assert!(fresh.save_path.is_none());
    assert!(fresh.pending_recovery.is_none());
}
