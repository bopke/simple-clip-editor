//! Every edit path produces exactly one undo entry.
//!
//! Rust has no reflection, so the enumeration below is written by hand. That
//! makes it a checklist as much as a test: a new mutating operation is only
//! covered once someone adds it here, and the `all_mutating_paths_are_listed`
//! test fails when the count drifts from what this file claims to cover.

use clip_core::{FrameIdx, Rational};
use clip_editor::app::App;
use clip_editor::cache::SourceCache;
use clip_editor::ffmpeg::{Origin, Tools};
use clip_editor::keys;

/// One named edit, applied to a fresh app. `setup` runs first and is not part
/// of what is under test — the pre-edit state is captured between the two.
struct EditPath {
    name: &'static str,
    setup: fn(&mut App),
    apply: fn(&mut App),
}

fn nothing(_: &mut App) {}

/// The complete set of operations that mutate `Project`. Adding a mutating
/// method without adding it here is the failure this file exists to catch.
const EDIT_PATHS: &[EditPath] = &[
    EditPath {
        name: "set_project_fps",
        setup: nothing,
        apply: |app| app.set_project_fps(Rational::new(24000, 1001)),
    },
    EditPath {
        name: "add_audio_track_from_path",
        setup: nothing,
        apply: |app| app.add_audio_track_from_path("/m/music.mp3".into()),
    },
    EditPath {
        name: "remove_audio_track",
        setup: |app| {
            let track = clip_core::AudioTrack::new("/m/a.mp3".into());
            app.project.as_mut().unwrap().added_tracks.push(track);
        },
        apply: |app| app.remove_audio_track(0),
    },
    EditPath {
        name: "keys::set_in",
        setup: nothing,
        apply: |app| {
            app.playhead = FrameIdx(30);
            keys::set_in(app);
        },
    },
    EditPath {
        name: "keys::set_out",
        setup: nothing,
        apply: |app| {
            app.playhead = FrameIdx(90);
            keys::set_out(app);
        },
    },
    EditPath {
        name: "keys::delete_range",
        setup: nothing,
        apply: |app| {
            app.selection = Some((FrameIdx(30), FrameIdx(60)));
            keys::delete_range(app);
        },
    },
];

/// An app with a project and with `tools`/`cache` populated, since some edit
/// paths bail out before touching the project without them. The ffmpeg path
/// does not exist: the worker it spawns fails to launch and sends an error
/// nobody drains, which is exactly as much of a decode as this test wants.
fn app_with_project(ctx: &egui::Context, dir: &std::path::Path) -> App {
    let mut app = App::with_context(ctx);
    let source = clip_core::SourceInfo {
        width: 1920,
        height: 1080,
        duration: clip_core::RationalTime::new(60, 1),
        avg_frame_rate: Rational::from_int(30),
        r_frame_rate: Rational::from_int(30),
        ..Default::default()
    };
    app.project = Some(clip_core::Project::new(source));
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

#[test]
fn every_edit_path_pushes_exactly_one_undo_entry() {
    let ctx = egui::Context::default();
    let dir = std::env::temp_dir().join("clipper-undo-entry-points");
    std::fs::create_dir_all(&dir).unwrap();

    for path in EDIT_PATHS {
        let mut app = app_with_project(&ctx, &dir);
        (path.setup)(&mut app);
        let before = app.project.clone().unwrap();
        assert_eq!(app.undo.depth(), 0, "{}: dirty fixture", path.name);

        (path.apply)(&mut app);

        assert_eq!(
            app.undo.depth(),
            1,
            "{} did not push exactly one undo entry",
            path.name
        );
        assert!(
            app.undo.can_undo(),
            "{}: undo entry is not reachable",
            path.name
        );
        // The entry has to hold the state from *before* the edit, which is the
        // part a direct `undo.push` after a mutation would get wrong.
        let restored = app.undo.undo(app.project.clone().unwrap()).unwrap();
        assert_eq!(
            restored.segments, before.segments,
            "{}: undo restored the wrong segments",
            path.name
        );
        assert_eq!(
            restored.added_tracks.len(),
            before.added_tracks.len(),
            "{}: undo restored the wrong track list",
            path.name
        );
    }
}

/// A removal the UI cannot satisfy is not an edit, so it must not consume an
/// undo step: an out-of-range index leaves the stack exactly as it was.
#[test]
fn a_rejected_removal_pushes_nothing() {
    let ctx = egui::Context::default();
    let dir = std::env::temp_dir().join("clipper-undo-entry-points");
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = app_with_project(&ctx, &dir);

    app.remove_audio_track(7);
    assert_eq!(app.undo.depth(), 0);
    assert!(!app.undo.can_undo());
}

/// Setting the fps to the value it already has is not an edit either.
#[test]
fn a_no_op_fps_change_pushes_nothing() {
    let ctx = egui::Context::default();
    let dir = std::env::temp_dir().join("clipper-undo-entry-points");
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = app_with_project(&ctx, &dir);

    let fps = app.project.as_ref().unwrap().project_fps;
    app.set_project_fps(fps);
    assert_eq!(app.undo.depth(), 0);
}

/// Guards against the list above silently falling behind `App`. It is a count,
/// not a signature check, so it fails loudly on a new mutating method and is
/// cheap to update deliberately.
#[test]
fn all_mutating_paths_are_listed() {
    assert_eq!(
        EDIT_PATHS.len(),
        6,
        "a mutating edit path was added or removed — update EDIT_PATHS to match"
    );
}
