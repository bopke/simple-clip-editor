//! The preview pane and the crop overlay.
//!
//! Preset and free crop are one interaction, not two: with a ratio locked
//! every handle constrains to it, dragging inside moves the rect, and Shift
//! temporarily unlocks. The rect snaps to even pixels here, in the UI, so what
//! the user sees is what they get.

use crate::app::{App, CropDrag};
use clip_core::crop::{self, Handle};
use clip_core::{AspectMode, CropRect};
use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

const HANDLE: f32 = 9.0;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let has_source = app
        .project
        .as_ref()
        .is_some_and(|p| p.source.display_size().0 > 0 && p.source.display_size().1 > 0);
    if !has_source {
        empty_state(app, ui);
        return;
    }
    let project = app.project.as_ref().expect("checked above");
    let (sw, sh) = project.source.display_size();

    let avail = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::from_rgb(12, 12, 14));

    // The frame is drawn at the output's aspect so there are no surprises at
    // export: pad mode shows its bars, crop shows only what survives.
    let pad = matches!(project.aspect_mode, AspectMode::PadToRatio(_));
    let (out_w, out_h) = project.output_size();
    let show_crop_tool = app.crop_tool;
    let crop_rect = project.crop;

    let display_aspect = if pad || (crop_rect.is_some() && !show_crop_tool) {
        out_w as f32 / out_h.max(1) as f32
    } else {
        sw as f32 / sh.max(1) as f32
    };
    let frame_rect = fit(rect.shrink(12.0), display_aspect);

    let Some(texture) = &app.texture else {
        painter.rect_filled(frame_rect, 4.0, Color32::from_rgb(20, 20, 24));
        painter.text(
            frame_rect.center(),
            egui::Align2::CENTER_CENTER,
            if app.proxy.is_some() {
                "decoding…"
            } else {
                "…"
            },
            egui::FontId::proportional(13.0),
            Color32::from_gray(120),
        );
        return;
    };

    if pad {
        // Letterbox/pillarbox: the bars are part of the output, so draw them.
        painter.rect_filled(frame_rect, 0.0, Color32::BLACK);
        let inner = fit(frame_rect, sw as f32 / sh.max(1) as f32);
        painter.image(
            texture.id(),
            inner,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    } else if let (Some(c), false) = (crop_rect, show_crop_tool) {
        let uv = Rect::from_min_max(
            Pos2::new(c.x as f32 / sw as f32, c.y as f32 / sh as f32),
            Pos2::new(
                (c.x + c.w) as f32 / sw as f32,
                (c.y + c.h) as f32 / sh as f32,
            ),
        );
        painter.image(texture.id(), frame_rect, uv, Color32::WHITE);
    } else {
        painter.image(
            texture.id(),
            frame_rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    if show_crop_tool {
        crop_overlay(app, ui, &response, frame_rect, (sw, sh));
    }

    // Live output dimensions — users want to confirm they got exactly
    // 1080x1920, and they want to see it before exporting, not after.
    let label = format!("{out_w} × {out_h}");
    let pos = frame_rect.right_bottom() + Vec2::new(-8.0, -8.0);
    let galley = ui.painter().layout_no_wrap(
        label,
        egui::FontId::monospace(12.0),
        Color32::from_gray(230),
    );
    let text_rect = egui::Align2::RIGHT_BOTTOM.anchor_size(pos, galley.size());
    ui.painter()
        .rect_filled(text_rect.expand(4.0), 3.0, Color32::from_black_alpha(160));
    ui.painter().galley(text_rect.min, galley, Color32::WHITE);
}

/// The whole empty pane is the open button — a big target that does the only
/// thing there is to do here.
fn empty_state(app: &mut App, ui: &mut egui::Ui) {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::from_rgb(12, 12, 14));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "Open a video to start",
        egui::FontId::proportional(16.0),
        if hovered {
            Color32::from_gray(210)
        } else {
            Color32::from_gray(140)
        },
    );
    painter.text(
        rect.center() + Vec2::new(0.0, 24.0),
        egui::Align2::CENTER_CENTER,
        "click here, drop a file, or press Ctrl+O",
        egui::FontId::proportional(12.0),
        Color32::from_gray(100),
    );
    if response.clicked() {
        app.open_dialog();
    }
}

/// Largest rect of `aspect` centred in `bounds`.
fn fit(bounds: Rect, aspect: f32) -> Rect {
    let (bw, bh) = (bounds.width(), bounds.height());
    let (w, h) = if bw / bh > aspect {
        (bh * aspect, bh)
    } else {
        (bw, bw / aspect)
    };
    Rect::from_center_size(bounds.center(), Vec2::new(w.max(1.0), h.max(1.0)))
}

fn crop_overlay(
    app: &mut App,
    ui: &mut egui::Ui,
    response: &egui::Response,
    frame_rect: Rect,
    (sw, sh): (u32, u32),
) {
    let scale = frame_rect.width() / sw as f32;
    let to_screen = |x: f32, y: f32| frame_rect.min + Vec2::new(x * scale, y * scale);
    let to_source = |p: Pos2| {
        (
            (p.x - frame_rect.min.x) / scale,
            (p.y - frame_rect.min.y) / scale,
        )
    };

    let current = app
        .project
        .as_ref()
        .and_then(|p| p.crop)
        .unwrap_or_else(|| crop::full_frame((sw, sh)));

    let r = Rect::from_min_max(
        to_screen(current.x as f32, current.y as f32),
        to_screen(
            (current.x + current.w) as f32,
            (current.y + current.h) as f32,
        ),
    );

    let painter = ui.painter_at(frame_rect);
    // Dim everything outside the crop.
    let shade = Color32::from_black_alpha(120);
    for outside in [
        Rect::from_min_max(frame_rect.min, Pos2::new(frame_rect.max.x, r.min.y)),
        Rect::from_min_max(Pos2::new(frame_rect.min.x, r.max.y), frame_rect.max),
        Rect::from_min_max(
            Pos2::new(frame_rect.min.x, r.min.y),
            Pos2::new(r.min.x, r.max.y),
        ),
        Rect::from_min_max(
            Pos2::new(r.max.x, r.min.y),
            Pos2::new(frame_rect.max.x, r.max.y),
        ),
    ] {
        if outside.is_positive() {
            painter.rect_filled(outside, 0.0, shade);
        }
    }
    painter.rect_stroke(
        r,
        0.0,
        Stroke::new(1.5, Color32::from_rgb(120, 200, 255)),
        StrokeKind::Middle,
    );
    // Thirds guides.
    for i in 1..3 {
        let f = i as f32 / 3.0;
        let x = r.min.x + r.width() * f;
        let y = r.min.y + r.height() * f;
        let g = Stroke::new(0.7, Color32::from_white_alpha(50));
        painter.line_segment([Pos2::new(x, r.min.y), Pos2::new(x, r.max.y)], g);
        painter.line_segment([Pos2::new(r.min.x, y), Pos2::new(r.max.x, y)], g);
    }
    for h in Handle::all_resize() {
        let p = handle_pos(r, h);
        painter.rect_filled(
            Rect::from_center_size(p, Vec2::splat(HANDLE)),
            2.0,
            Color32::from_rgb(120, 200, 255),
        );
    }

    // -- interaction ---------------------------------------------------
    let ratio = app
        .project
        .as_ref()
        .and_then(|p| p.aspect_mode.locked_ratio());
    // Shift temporarily unlocks the ratio.
    let shift = ui.input(|i| i.modifiers.shift);
    let ratio = if shift { None } else { ratio };

    if response.drag_started() {
        if let Some(pos) = response.interact_pointer_pos() {
            let handle = Handle::all_resize()
                .into_iter()
                .find(|h| {
                    Rect::from_center_size(handle_pos(r, *h), Vec2::splat(HANDLE * 2.0))
                        .contains(pos)
                })
                .or_else(|| r.contains(pos).then_some(Handle::Move));
            if let Some(handle) = handle {
                app.snapshot();
                app.crop_drag = Some(CropDrag {
                    handle,
                    grab: pos - r.min,
                });
            }
        }
    }

    if response.dragged() {
        if let (Some(drag), Some(pos)) = (&app.crop_drag, response.interact_pointer_pos()) {
            let (sx, sy) = to_source(pos);
            let next = if drag.handle == Handle::Move {
                let origin = pos - drag.grab;
                let (ox, oy) = to_source(origin);
                crop::translate(
                    current,
                    ox - current.x as f32,
                    oy - current.y as f32,
                    (sw, sh),
                )
            } else {
                crop::drag_handle(current, drag.handle, sx, sy, ratio, (sw, sh))
            };
            set_crop(app, next);
        }
    }

    if response.drag_stopped() {
        app.crop_drag = None;
    }

    let readout = crop::describe(current);
    let pos = frame_rect.left_top() + Vec2::new(8.0, 8.0);
    ui.painter().text(
        pos,
        egui::Align2::LEFT_TOP,
        readout,
        egui::FontId::monospace(12.0),
        Color32::from_gray(240),
    );
}

fn handle_pos(r: Rect, h: Handle) -> Pos2 {
    match h {
        Handle::NW => r.left_top(),
        Handle::N => Pos2::new(r.center().x, r.min.y),
        Handle::NE => r.right_top(),
        Handle::E => Pos2::new(r.max.x, r.center().y),
        Handle::SE => r.right_bottom(),
        Handle::S => Pos2::new(r.center().x, r.max.y),
        Handle::SW => r.left_bottom(),
        Handle::W => Pos2::new(r.min.x, r.center().y),
        Handle::Move => r.center(),
    }
}

pub fn set_crop(app: &mut App, rect: CropRect) {
    if let Some(p) = app.project.as_mut() {
        p.crop = Some(rect);
    }
}
