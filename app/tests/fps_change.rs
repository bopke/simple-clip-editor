//! Changing the project frame rate re-views the edit; it does not rewrite it.
//!
//! The cases that matter are the ones where the two grids do not divide each
//! other — 30 against 23.976 — because that is where rescaling frame indices
//! loses a frame it can never get back.

use clip_core::{FrameIdx, Rational, RationalTime, Segment};
use clip_editor::app::App;

fn ntsc_24() -> Rational {
    Rational::new(24000, 1001)
}

fn fps_30() -> Rational {
    Rational::from_int(30)
}

/// An app at 30 fps over a two-minute source. No tools and no cache, so the
/// re-import a rate change triggers returns immediately.
fn app_at_30(ctx: &egui::Context) -> App {
    let mut app = App::with_context(ctx);
    let source = clip_core::SourceInfo {
        width: 1920,
        height: 1080,
        duration: RationalTime::new(120, 1),
        avg_frame_rate: fps_30(),
        r_frame_rate: fps_30(),
        ..Default::default()
    };
    app.project = Some(clip_core::Project::new(source));
    app
}

/// #2: cuts survive a round-trip through a rate that cannot represent them.
///
/// Frames 2 and 3 at 30 fps both land on frame 2 at 23.976. Rescaling indices
/// gives one of them back wrong no matter how exact the arithmetic is; storing
/// the boundary as a time means neither is ever converted in the first place.
#[test]
fn segment_boundaries_round_trip_through_23_976() {
    let ctx = egui::Context::default();
    let mut app = app_at_30(&ctx);

    let frames = [(0u64, 2u64), (3, 97), (100, 1801), (1802, 3600)];
    let original: Vec<Segment> = frames
        .iter()
        .map(|(a, b)| Segment::from_frames(FrameIdx(*a), FrameIdx(*b), fps_30()))
        .collect();
    app.project.as_mut().unwrap().segments = original.clone();

    app.set_project_fps(ntsc_24());
    app.set_project_fps(fps_30());

    assert_eq!(app.project.as_ref().unwrap().segments, original);
    // And the frames they read back as are the frames they were authored from.
    for (seg, (a, b)) in app
        .project
        .as_ref()
        .unwrap()
        .segments
        .iter()
        .zip(frames.iter())
    {
        assert_eq!(seg.frame_span(fps_30()), (FrameIdx(*a), FrameIdx(*b)));
    }
}

/// Repeated conversions must not accumulate error either — the failure mode of
/// `f64` rescaling was a frame of drift per conversion, which only shows up
/// after several.
#[test]
fn many_rate_changes_do_not_drift() {
    let ctx = egui::Context::default();
    let mut app = app_at_30(&ctx);
    let original = vec![Segment::from_frames(
        FrameIdx(137),
        FrameIdx(2003),
        fps_30(),
    )];
    app.project.as_mut().unwrap().segments = original.clone();

    for _ in 0..20 {
        app.set_project_fps(ntsc_24());
        app.set_project_fps(Rational::from_int(25));
        app.set_project_fps(Rational::new(30000, 1001));
        app.set_project_fps(fps_30());
    }

    assert_eq!(app.project.as_ref().unwrap().segments, original);
}

/// #1: the selection is an edit range too, and used to be left at the old rate
/// entirely — after a change it pointed at different content than it did
/// before, while the timeline kept drawing it as if it were still valid.
#[test]
fn the_selection_keeps_its_wall_clock_range() {
    let ctx = egui::Context::default();
    let mut app = app_at_30(&ctx);
    // Source seconds 10 to 20.
    app.selection = Some((FrameIdx(300), FrameIdx(600)));
    app.playhead = FrameIdx(450);

    app.set_project_fps(ntsc_24());

    let (a, b) = app.selection.expect("selection survives the rate change");
    let seconds = |f: FrameIdx| f.to_time(ntsc_24()).as_f64();
    // Within half a frame of the new grid, which is all a coarser grid can do.
    let half_frame = 0.5 / ntsc_24().as_f64();
    assert!(
        (seconds(a) - 10.0).abs() <= half_frame,
        "in at {}",
        seconds(a)
    );
    assert!(
        (seconds(b) - 20.0).abs() <= half_frame,
        "out at {}",
        seconds(b)
    );
    assert!((seconds(app.playhead) - 15.0).abs() <= half_frame);
}

/// The selection is a cursor, not an edit point, so it is allowed to quantise
/// onto a coarser grid — but it must still be undoable back to the frame it
/// was set on, which is what the snapshot in `set_project_fps` is for.
#[test]
fn an_fps_change_can_be_undone() {
    let ctx = egui::Context::default();
    let mut app = app_at_30(&ctx);
    app.project.as_mut().unwrap().segments =
        vec![Segment::from_frames(FrameIdx(7), FrameIdx(53), fps_30())];

    app.set_project_fps(ntsc_24());
    app.undo();

    assert_eq!(
        app.project.as_ref().unwrap().segments[0].frame_span(fps_30()),
        (FrameIdx(7), FrameIdx(53))
    );
}

/// The output is measured on the grid being viewed, so the same edit is a
/// different number of frames at a different rate — and the same duration.
#[test]
fn the_output_length_follows_the_grid() {
    let ctx = egui::Context::default();
    let mut app = app_at_30(&ctx);
    app.project.as_mut().unwrap().segments =
        vec![Segment::from_frames(FrameIdx(0), FrameIdx(300), fps_30())];

    assert_eq!(app.project.as_ref().unwrap().output_frames(), 300);
    app.set_project_fps(ntsc_24());
    assert_eq!(app.project.as_ref().unwrap().output_frames(), 240);

    let seconds = app.project.as_ref().unwrap().output_duration().as_f64();
    assert!((seconds - 10.0).abs() < 0.01, "{seconds}");
}
