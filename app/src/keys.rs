//! Keyboard-first interaction. Not polish — tools that feel good feel good
//! because your hands don't leave the keyboard, so this is wired in from the
//! start rather than retrofitted onto mouse-centric code.

use crate::app::App;
use clip_core::{edits, FrameIdx, RationalTime};
use egui::Key;

/// Rows for the shortcuts panel, kept next to the handler so the two cannot
/// drift apart.
pub const HELP: &[(&str, &str)] = &[
    ("Space", "Play / pause"),
    ("J / K / L", "Reverse / stop / forward (repeat for faster)"),
    ("← / →", "Step one frame"),
    ("Shift + ← / →", "Step one second"),
    ("I / O", "Set in / out point at playhead"),
    ("X", "Delete range under playhead"),
    ("Home / End", "Start / end of timeline"),
    ("C", "Toggle crop tool"),
    ("Ctrl+Z / Ctrl+Shift+Z", "Undo / redo"),
    ("Ctrl+S / Ctrl+Shift+S", "Save project / save as"),
    ("Ctrl+O", "Open a project or a video"),
    ("Ctrl+E", "Export"),
    ("Esc", "Cancel drag / close dialog"),
];

pub fn handle(app: &mut App, ctx: &egui::Context) {
    // Don't steal keys from a text field.
    if ctx.egui_wants_keyboard_input() {
        return;
    }
    // Modifiers come from the event, not from the global input state: the
    // two can disagree, and the event is what the user actually pressed.
    let pressed: Vec<(Key, egui::Modifiers)> = ctx.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => Some((*key, *modifiers)),
                _ => None,
            })
            .collect()
    });

    for (key, mods) in pressed {
        match key {
            Key::Escape => {
                if app.crop_drag.take().is_none() {
                    if app.show_export {
                        app.show_export = false;
                    } else if app.export_job.is_some() {
                        app.cancel_export();
                    } else {
                        app.crop_tool = false;
                    }
                }
            }
            Key::O if mods.command => app.open_dialog(),
            Key::S if mods.command && mods.shift => app.save_project_as(),
            Key::S if mods.command => app.save_project(),
            Key::E if mods.command => app.show_export = true,
            Key::Z if mods.command && mods.shift => app.redo(),
            Key::Z if mods.command => app.undo(),
            Key::Space => app.toggle_play(),
            Key::L => shuttle(app, 1),
            Key::J => shuttle(app, -1),
            Key::K => {
                app.shuttle = 0;
                app.pause();
            }
            Key::ArrowLeft => {
                let step = if mods.shift {
                    seconds_in_frames(app)
                } else {
                    1
                };
                app.step(-(step as i64));
            }
            Key::ArrowRight => {
                let step = if mods.shift {
                    seconds_in_frames(app)
                } else {
                    1
                };
                app.step(step as i64);
            }
            Key::Home => {
                app.pause();
                let fps = app.fps();
                let first = app
                    .project
                    .as_ref()
                    .and_then(|p| p.normalised_segments().first().map(|s| s.start_frame(fps)))
                    .unwrap_or(FrameIdx(0));
                app.seek_source(first);
            }
            Key::End => {
                app.pause();
                let fps = app.fps();
                let last = app
                    .project
                    .as_ref()
                    .and_then(|p| p.normalised_segments().last().map(|s| s.end_frame(fps)))
                    .map(|f| f.saturating_sub(1))
                    .unwrap_or(FrameIdx(0));
                app.seek_source(last);
            }
            Key::I => set_in(app),
            Key::O => set_out(app),
            Key::X => delete_range(app),
            Key::C => app.crop_tool = !app.crop_tool,
            _ => {}
        }
    }
}

/// One second as a whole number of frames — the Shift+arrow jump. Through the
/// rate rather than through `fps.as_f64().round()`, so 29.97 is the 30 frames
/// that actually make up a second of it rather than a rounded rate.
fn seconds_in_frames(app: &App) -> u64 {
    let fps = app.fps();
    FrameIdx::from_time_round(RationalTime::new(1, 1), fps)
        .0
        .max(1)
}

/// `L` repeatedly goes faster; `J` the same in reverse. Only ±1 plays audio —
/// shuttling is a scrub, and pitch-shifted audio helps nobody.
fn shuttle(app: &mut App, direction: i32) {
    let current = app.shuttle;
    let next = if current.signum() != direction {
        direction
    } else {
        (current * 2).clamp(-8, 8)
    };
    app.shuttle = next;
    if next == 1 {
        app.play();
    } else {
        app.player.pause();
        if let Some(s) = app.video_stream.take() {
            s.stop();
        }
    }
}

pub fn set_in(app: &mut App) {
    let at = app.playhead.to_time(app.fps());
    app.edit(|p| edits::set_in(&mut p.segments, at));
}

pub fn set_out(app: &mut App) {
    // The playhead frame is kept, so the out-point is where the next frame
    // would start.
    let end = app.playhead.saturating_add(1).to_time(app.fps());
    app.edit(|p| edits::set_out(&mut p.segments, end));
}

/// With a selection, `X` removes exactly that range. Without one, it removes
/// from the playhead to the end of the segment the playhead is in.
pub fn delete_range(app: &mut App) {
    let fps = app.fps();
    let playhead = app.playhead.to_time(fps);
    let range = match app.selection {
        Some((a, b)) if b.0 > a.0 => Some((a.to_time(fps), b.to_time(fps))),
        _ => app.project.as_ref().and_then(|p| {
            let segs = p.normalised_segments();
            edits::segment_at(&segs, playhead).map(|i| (playhead, segs[i].end))
        }),
    };
    let Some((from, to)) = range else { return };
    app.edit(|p| edits::delete_range(&mut p.segments, from, to));
    app.selection = None;
}

/// Per-frame shuttle stepping for speeds other than ±1.
pub fn tick_shuttle(app: &mut App, ctx: &egui::Context) {
    if app.shuttle == 0 || app.shuttle == 1 {
        return;
    }
    let step = app.shuttle as i64;
    let target = if step < 0 {
        app.playhead.saturating_sub((-step) as u64)
    } else {
        app.playhead.saturating_add(step as u64)
    };
    if target.0 == 0 && step < 0 {
        app.shuttle = 0;
    }
    app.seek_source(target);
    ctx.request_repaint();
}
