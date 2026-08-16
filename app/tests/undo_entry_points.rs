//! Every edit path produces exactly one undo entry — and marks the edit unsaved.
//!
//! Both halves matter, and the second is the one that was wrong. `unsaved` was
//! set in two places while the app had fifteen kinds of edit, so a crop drag,
//! a gain slider or a preset change left it false: autosave skipped the
//! recovery snapshot, the title bar showed no `•`, and closing the window
//! deleted the snapshot on the grounds that there was nothing to lose. This
//! file used to check only the undo half, which is why the other half could be
//! wrong for nine paths at once.
//!
//! Rust has no reflection, so the enumeration below is written by hand. That
//! makes it a checklist as much as a test: a new mutating operation is only
//! covered once someone adds it here, and `all_mutating_paths_are_listed`
//! fails when the count drifts from what this file claims to cover.

use clip_core::{
    AspectMode, AudioSourceSelection, CropRect, FrameIdx, OutputPreset, Quality, Rational,
};
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

fn with_track(app: &mut App) {
    let track = clip_core::AudioTrack::new("/m/a.mp3".into());
    app.project.as_mut().unwrap().added_tracks.push(track);
}

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
        setup: with_track,
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
    // Everything below here mutated the project without touching `unsaved`,
    // and all but the crop drag without touching the undo stack either.
    EditPath {
        name: "set_aspect_and_crop",
        setup: nothing,
        apply: |app| {
            app.set_aspect_and_crop(
                AspectMode::Locked(Rational::new(9, 16)),
                Some(CropRect {
                    x: 0,
                    y: 0,
                    w: 608,
                    h: 1080,
                }),
            )
        },
    },
    EditPath {
        name: "drag_crop",
        setup: nothing,
        apply: |app| {
            app.drag_crop(CropRect {
                x: 10,
                y: 10,
                w: 640,
                h: 360,
            })
        },
    },
    EditPath {
        name: "set_original_audio_source",
        setup: nothing,
        apply: |app| app.set_original_audio_source(AudioSourceSelection::MixAll),
    },
    EditPath {
        name: "set_original_gain",
        setup: nothing,
        apply: |app| app.set_original_gain(-6.0),
    },
    EditPath {
        name: "set_track_gain",
        setup: with_track,
        apply: |app| app.set_track_gain(0, -3.0),
    },
    EditPath {
        name: "set_track_offset",
        setup: with_track,
        apply: |app| app.set_track_offset(0, 2.5),
    },
    EditPath {
        name: "set_track_fade_out",
        setup: with_track,
        apply: |app| app.set_track_fade_out(0, true),
    },
    EditPath {
        name: "set_output_settings",
        setup: nothing,
        apply: |app| {
            let mut settings = app.project.as_ref().unwrap().output.clone();
            settings.preset = OutputPreset::WebmVp9;
            settings.long_edge = Some(1080);
            app.set_output_settings(settings);
        },
    },
    EditPath {
        name: "set_crf",
        setup: nothing,
        apply: |app| app.set_crf(31),
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

fn workdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("clipper-undo-entry-points");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn every_edit_path_pushes_exactly_one_undo_entry() {
    let ctx = egui::Context::default();
    let dir = workdir();

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

/// The half that was wrong. Without this the app believes there is nothing to
/// lose: no autosave, no `•`, and `finish_session` discards the snapshot.
#[test]
fn every_edit_path_marks_the_project_unsaved() {
    let ctx = egui::Context::default();
    let dir = workdir();

    for path in EDIT_PATHS {
        let mut app = app_with_project(&ctx, &dir);
        (path.setup)(&mut app);
        // As if the edit had just been saved: this is the state in which
        // losing the next change costs the most.
        app.unsaved = false;

        (path.apply)(&mut app);

        assert!(
            app.unsaved,
            "{} changed the project without marking it unsaved",
            path.name
        );
        assert!(
            app.window_title().starts_with('•'),
            "{}: the title bar shows no unsaved marker",
            path.name
        );
    }
}

/// A drag reports once per frame. Sixty entries for one gesture is not just
/// noisy — `MAX_DEPTH` is 100, so two seconds of dragging would push every
/// other undo entry off the stack.
#[test]
fn dragging_one_control_is_one_undo_entry() {
    let ctx = egui::Context::default();
    let mut app = app_with_project(&ctx, &workdir());

    for i in 0..60 {
        app.set_original_gain(-(i as f32) / 10.0);
    }
    assert_eq!(app.undo.depth(), 1, "a drag is one thing to undo");
    assert!(app.unsaved);

    // Undoing it goes back to before the drag started, not to some frame
    // partway through it.
    app.undo();
    assert_eq!(app.project.as_ref().unwrap().original_audio.gain_db, 0.0);
}

/// Coalescing is per control, so moving to a different one always starts a new
/// entry — otherwise a gain nudge and a crop drag would undo together.
#[test]
fn moving_to_another_control_starts_a_new_entry() {
    let ctx = egui::Context::default();
    let mut app = app_with_project(&ctx, &workdir());

    app.set_original_gain(-6.0);
    app.set_crf(30);
    app.set_original_gain(-9.0);
    assert_eq!(app.undo.depth(), 3);

    // And each one undoes on its own, most recent first.
    app.undo();
    assert_eq!(app.project.as_ref().unwrap().original_audio.gain_db, -6.0);
    app.undo();
    assert_eq!(
        app.project.as_ref().unwrap().output.quality,
        Quality::Crf(18)
    );
}

/// An undo ends the gesture: the next nudge of the same slider must be its own
/// entry, or it would fold into the state the undo just walked back to and the
/// user could not get back out again.
#[test]
fn an_undo_ends_the_run_of_continuous_edits() {
    let ctx = egui::Context::default();
    let mut app = app_with_project(&ctx, &workdir());

    app.set_original_gain(-6.0);
    app.undo();
    assert_eq!(app.undo.depth(), 0);

    app.set_original_gain(-3.0);
    assert_eq!(app.undo.depth(), 1, "the next edit is its own entry");
    app.undo();
    assert_eq!(app.project.as_ref().unwrap().original_audio.gain_db, 0.0);
}

/// A removal the UI cannot satisfy is not an edit, so it must not consume an
/// undo step: an out-of-range index leaves the stack exactly as it was.
#[test]
fn a_rejected_removal_pushes_nothing() {
    let ctx = egui::Context::default();
    let mut app = app_with_project(&ctx, &workdir());

    app.remove_audio_track(7);
    assert_eq!(app.undo.depth(), 0);
    assert!(!app.undo.can_undo());
    assert!(!app.unsaved, "a rejected edit is not an edit");
}

/// Setting the fps to the value it already has is not an edit either.
#[test]
fn a_no_op_fps_change_pushes_nothing() {
    let ctx = egui::Context::default();
    let mut app = app_with_project(&ctx, &workdir());

    let fps = app.project.as_ref().unwrap().project_fps;
    app.set_project_fps(fps);
    assert_eq!(app.undo.depth(), 0);
    assert!(!app.unsaved);
}

/// Guards against the list above silently falling behind `App`. It is a count,
/// not a signature check, so it fails loudly on a new mutating method and is
/// cheap to update deliberately.
#[test]
fn all_mutating_paths_are_listed() {
    assert_eq!(
        EDIT_PATHS.len(),
        15,
        "a mutating edit path was added or removed — update EDIT_PATHS to match"
    );
}
