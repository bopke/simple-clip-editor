//! Headless UI tests. egui is immediate-mode, so running the real layout with
//! a real project exercises every paint and hit-test path — which is where id
//! clashes, degenerate rects and panics from an empty timeline show up.

use clip_core::{AspectMode, CropRect, FrameIdx, Rational, Segment};
use clip_editor::app::App;
use clip_editor::ui_root::root_ui;

/// Frames are 1/60 s apart. egui's press-to-drag transition is time-aware, so
/// a harness with a frozen clock silently never produces a drag.
static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Run one frame of the whole UI at a given window size.
fn frame(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>) {
    let tick = CLOCK.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1280.0, 820.0),
        )),
        time: Some(tick as f64 / 60.0),
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| root_ui(app, ui));
    // No renderer here, so the texture deltas are ours to discard.
    output.textures_delta.clear();
}

fn key(k: egui::Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
    vec![egui::Event::Key {
        key: k,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }]
}

/// The fixture source runs at 60 fps; segments are named by its frames.
fn seg(start: u64, end: u64) -> Segment {
    Segment::from_frames(FrameIdx(start), FrameIdx(end), Rational::from_int(60))
}

fn test_app(ctx: &egui::Context) -> App {
    let mut app = App::with_context(ctx);
    // The first-run screen is what shows without ffmpeg; the editor proper
    // needs the tools present, so skip these tests when they are not.
    if app.tools.is_none() {
        app.boot = clip_editor::app::Boot::Missing;
    }
    app
}

/// A stand-in for a decoded frame. The preview draws nothing without one, so
/// anything testing the crop overlay needs it.
fn fake_frame(app: &mut App, ctx: &egui::Context) {
    let image = egui::ColorImage::from_rgba_unmultiplied([16, 9], &[128u8; 16 * 9 * 4]);
    app.texture = Some(ctx.load_texture("test", image, egui::TextureOptions::LINEAR));
    app.texture_size = (16, 9);
}

fn project(app: &mut App) {
    let mut source = clip_core::SourceInfo {
        width: 1920,
        height: 1080,
        duration: clip_core::RationalTime::new(60, 1),
        avg_frame_rate: Rational::from_int(60),
        r_frame_rate: Rational::from_int(60),
        ..Default::default()
    };
    source.audio_streams.push(clip_core::AudioStreamInfo {
        index: 0,
        codec: "aac".into(),
        channels: 2,
        sample_rate: 48_000,
        language: None,
        title: None,
    });
    app.project = Some(clip_core::Project::new(source));
    app.boot = clip_editor::app::Boot::Ready;
}

#[test]
fn renders_the_empty_state_without_a_project() {
    let ctx = egui::Context::default();
    let mut app = test_app(&ctx);
    app.boot = clip_editor::app::Boot::Ready;
    for _ in 0..3 {
        frame(&mut app, &ctx, vec![]);
    }
}

#[test]
fn renders_the_first_run_screen() {
    let ctx = egui::Context::default();
    let mut app = test_app(&ctx);
    app.boot = clip_editor::app::Boot::Missing;
    frame(&mut app, &ctx, vec![]);
    app.boot = clip_editor::app::Boot::Downloading {
        done: 5_000_000,
        total: 45_000_000,
        cancel: clip_editor::ffmpeg::Cancel::new(),
    };
    frame(&mut app, &ctx, vec![]);
    app.boot = clip_editor::app::Boot::Failed("checksum mismatch".into());
    frame(&mut app, &ctx, vec![]);
}

#[test]
fn renders_a_full_editor_with_crop_and_cuts() {
    let ctx = egui::Context::default();
    let mut app = test_app(&ctx);
    project(&mut app);
    if let Some(p) = app.project.as_mut() {
        p.segments = vec![seg(0, 600), seg(1200, 1800)];
        p.crop = Some(CropRect {
            x: 656,
            y: 0,
            w: 608,
            h: 1080,
        });
        p.aspect_mode = AspectMode::Locked(Rational::new(9, 16));
    }
    app.crop_tool = true;
    app.selection = Some((FrameIdx(100), FrameIdx(300)));
    app.playhead = FrameIdx(150);
    for _ in 0..3 {
        frame(&mut app, &ctx, vec![]);
    }

    // Pad mode draws its bars instead of cropping.
    if let Some(p) = app.project.as_mut() {
        p.aspect_mode = AspectMode::PadToRatio(Rational::new(9, 16));
    }
    frame(&mut app, &ctx, vec![]);
}

#[test]
fn an_empty_timeline_does_not_panic() {
    let ctx = egui::Context::default();
    let mut app = test_app(&ctx);
    project(&mut app);
    // Every frame deleted: the app must still draw, and export must refuse
    // rather than produce something.
    if let Some(p) = app.project.as_mut() {
        p.segments.clear();
    }
    for _ in 0..2 {
        frame(&mut app, &ctx, vec![]);
    }
    assert!(app.timeline().is_empty());
    app.play();
    assert!(!app.player.is_playing(), "nothing to play");
}

#[test]
fn keyboard_edits_the_timeline() {
    let ctx = egui::Context::default();
    let mut app = test_app(&ctx);
    project(&mut app);
    frame(&mut app, &ctx, vec![]);

    // → steps one frame, Shift+→ steps a second.
    frame(
        &mut app,
        &ctx,
        key(egui::Key::ArrowRight, egui::Modifiers::NONE),
    );
    assert_eq!(app.playhead, FrameIdx(1));
    frame(
        &mut app,
        &ctx,
        key(egui::Key::ArrowRight, egui::Modifiers::SHIFT),
    );
    assert_eq!(app.playhead, FrameIdx(61));

    // I trims the in-point to the playhead.
    frame(&mut app, &ctx, key(egui::Key::I, egui::Modifiers::NONE));
    assert_eq!(
        app.project.as_ref().unwrap().segments[0].start_frame(Rational::from_int(60)),
        FrameIdx(61)
    );

    // X with no selection removes the rest of the segment.
    frame(&mut app, &ctx, key(egui::Key::X, egui::Modifiers::NONE));
    assert!(app
        .project
        .as_ref()
        .unwrap()
        .normalised_segments()
        .is_empty());

    // Undo puts it back.
    frame(&mut app, &ctx, key(egui::Key::Z, egui::Modifiers::COMMAND));
    assert_eq!(app.project.as_ref().unwrap().segments[0], seg(61, 3600));

    // C toggles the crop tool.
    assert!(!app.crop_tool);
    frame(&mut app, &ctx, key(egui::Key::C, egui::Modifiers::NONE));
    assert!(app.crop_tool);
}

#[test]
fn home_and_end_go_to_the_edges_of_the_edit() {
    let ctx = egui::Context::default();
    let mut app = test_app(&ctx);
    project(&mut app);
    if let Some(p) = app.project.as_mut() {
        p.segments = vec![seg(300, 900)];
    }
    frame(&mut app, &ctx, key(egui::Key::End, egui::Modifiers::NONE));
    assert_eq!(app.playhead, FrameIdx(899));
    frame(&mut app, &ctx, key(egui::Key::Home, egui::Modifiers::NONE));
    assert_eq!(app.playhead, FrameIdx(300));
}

#[test]
fn the_export_dialog_opens_and_closes() {
    let ctx = egui::Context::default();
    let mut app = test_app(&ctx);
    project(&mut app);
    frame(&mut app, &ctx, key(egui::Key::E, egui::Modifiers::COMMAND));
    assert!(app.show_export);
    frame(&mut app, &ctx, vec![]);
    frame(
        &mut app,
        &ctx,
        key(egui::Key::Escape, egui::Modifiers::NONE),
    );
    assert!(!app.show_export);
}

/// Drag the pointer from `from` to `to` over several frames, the way a real
/// drag arrives: press, move, move, release.
fn drag(app: &mut App, ctx: &egui::Context, from: egui::Pos2, to: egui::Pos2) {
    frame(app, ctx, vec![egui::Event::PointerMoved(from)]);
    frame(
        app,
        ctx,
        vec![egui::Event::PointerButton {
            pos: from,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    let steps = 4;
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        let p = from + (to - from) * t;
        frame(app, ctx, vec![egui::Event::PointerMoved(p)]);
    }
    frame(
        app,
        ctx,
        vec![egui::Event::PointerButton {
            pos: to,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );
}

#[test]
fn dragging_inside_the_crop_rect_moves_it() {
    let ctx = egui::Context::default();
    let mut app = test_app(&ctx);
    project(&mut app);
    // A 9:16 crop starts centred; the point of this is being able to move it
    // off centre, because the subject usually is not in the middle.
    if let Some(p) = app.project.as_mut() {
        p.aspect_mode = AspectMode::Locked(Rational::new(9, 16));
        p.crop = Some(clip_core::crop::centered_with_ratio(
            (1920, 1080),
            Rational::new(9, 16),
        ));
    }
    app.crop_tool = true;
    fake_frame(&mut app, &ctx);
    frame(&mut app, &ctx, vec![]);

    let before = app.project.as_ref().unwrap().crop.unwrap();
    assert_eq!(before.x, 656, "starts centred");

    // The preview occupies the middle of the window; its centre is inside the
    // crop rect, so this is a move drag, not a resize.
    let centre = egui::pos2(490.0, 400.0);
    drag(&mut app, &ctx, centre, centre - egui::vec2(90.0, 0.0));

    let after = app.project.as_ref().unwrap().crop.unwrap();
    assert!(
        after.x < before.x,
        "moved left: {} -> {}",
        before.x,
        after.x
    );
    assert_eq!(after.w, before.w, "size unchanged by a move");
    assert_eq!(after.h, before.h);
    assert_eq!(
        after.y, before.y,
        "horizontal drag does not move it vertically"
    );
    // Clamping to the frame edges is covered by crop::translate's own tests;
    // dragging the pointer off-screen here would only test the harness.
}
