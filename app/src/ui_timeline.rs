//! The timeline: thumbnail strip, waveform, kept/cut ranges, playhead.
//!
//! For trimming, the waveform is often better than audio for finding cut
//! points — the gunshot, the laugh, the silence are all visible. Combined with
//! frame-stepping, precise cuts are possible with no audio at all.

use crate::app::App;
use clip_core::{FrameIdx, RationalTime};
use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

const THUMB_H: f32 = 44.0;
const WAVE_H: f32 = 56.0;
const SEG_H: f32 = 16.0;
const RULER_H: f32 = 16.0;

pub fn height() -> f32 {
    THUMB_H + WAVE_H + SEG_H + RULER_H + 16.0
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let total = app.source_frames();
    if app.project.is_none() || total == 0 {
        ui.allocate_space(ui.available_size());
        return;
    }
    let width = ui.available_width();
    let rect = ui
        .allocate_exact_size(Vec2::new(width, height() - 8.0), Sense::hover())
        .0;

    let thumb_rect = Rect::from_min_size(rect.min, Vec2::new(rect.width(), THUMB_H));
    let wave_rect = Rect::from_min_size(
        Pos2::new(rect.min.x, thumb_rect.max.y + 2.0),
        Vec2::new(rect.width(), WAVE_H),
    );
    let seg_rect = Rect::from_min_size(
        Pos2::new(rect.min.x, wave_rect.max.y + 2.0),
        Vec2::new(rect.width(), SEG_H),
    );
    let ruler_rect = Rect::from_min_size(
        Pos2::new(rect.min.x, seg_rect.max.y + 2.0),
        Vec2::new(rect.width(), RULER_H),
    );

    let x_of = |frame: u64| -> f32 { rect.min.x + (frame as f32 / total as f32) * rect.width() };
    let frame_of = |x: f32| -> FrameIdx {
        let f = ((x - rect.min.x) / rect.width().max(1.0)) * total as f32;
        FrameIdx((f.max(0.0) as u64).min(total.saturating_sub(1)))
    };

    draw_thumbs(app, ui, thumb_rect);
    draw_waveform(app, ui, wave_rect);
    draw_segments(app, ui, seg_rect, &x_of);
    draw_ruler(app, ui, ruler_rect, &x_of);
    dim_cuts(
        app,
        ui,
        Rect::from_min_max(thumb_rect.min, wave_rect.max),
        &x_of,
    );
    draw_selection(
        app,
        ui,
        Rect::from_min_max(thumb_rect.min, seg_rect.max),
        &x_of,
    );
    draw_playhead(app, ui, Rect::from_min_max(rect.min, ruler_rect.max), &x_of);

    // -- interaction ---------------------------------------------------
    let scrub_area = Rect::from_min_max(thumb_rect.min, wave_rect.max);
    let scrub = ui.interact(
        scrub_area,
        ui.id().with("timeline-scrub"),
        Sense::click_and_drag(),
    );
    if scrub.is_pointer_button_down_on() || scrub.clicked() {
        if let Some(pos) = scrub.interact_pointer_pos() {
            app.pause();
            let f = frame_of(pos.x);
            app.seek_source(f);
        }
    }

    // The thin band under the waveform is where ranges are marked, so a scrub
    // and a selection can never be confused for one another.
    let select = ui.interact(
        seg_rect,
        ui.id().with("timeline-select"),
        Sense::click_and_drag(),
    );
    if select.drag_started() {
        if let Some(pos) = select.interact_pointer_pos() {
            let f = frame_of(pos.x);
            app.selection = Some((f, f));
        }
    }
    if select.dragged() {
        if let (Some((start, _)), Some(pos)) = (app.selection, select.interact_pointer_pos()) {
            let f = frame_of(pos.x);
            app.selection = Some((FrameIdx(start.0.min(f.0)), FrameIdx(start.0.max(f.0))));
        }
    }
    if select.clicked() && !select.dragged() {
        app.selection = None;
    }
}

fn draw_thumbs(app: &mut App, ui: &mut egui::Ui, rect: Rect) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_rgb(18, 19, 22));
    if app.thumbs.is_empty() {
        return;
    }
    if app.thumb_textures.len() != app.thumbs.len() {
        load_thumbs(app, ui.ctx());
    }
    let n = app.thumb_textures.len().max(1);
    let w = rect.width() / n as f32;
    for (i, tex) in app.thumb_textures.iter().enumerate() {
        let r = Rect::from_min_size(
            Pos2::new(rect.min.x + i as f32 * w, rect.min.y),
            Vec2::new(w + 1.0, rect.height()),
        );
        painter.image(
            tex.id(),
            r,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

fn load_thumbs(app: &mut App, ctx: &egui::Context) {
    app.thumb_textures.clear();
    for path in &app.thumbs {
        let Ok(img) = image::open(path) else { continue };
        let img = img.to_rgba8();
        let size = [img.width() as usize, img.height() as usize];
        let image = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        app.thumb_textures.push(ctx.load_texture(
            path.to_string_lossy(),
            image,
            egui::TextureOptions::LINEAR,
        ));
    }
}

fn draw_waveform(app: &App, ui: &mut egui::Ui, rect: Rect) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_rgb(16, 17, 19));
    let Some(wf) = &app.waveform else {
        if app.project.as_ref().is_some_and(|p| p.has_audio()) {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "analysing audio…",
                egui::FontId::proportional(11.0),
                Color32::from_gray(90),
            );
        }
        return;
    };
    let mid = rect.center().y;
    let half = rect.height() / 2.0 - 2.0;
    let columns = rect.width() as usize;
    let buckets = wf.buckets().max(1);
    let color = Color32::from_rgb(90, 150, 200);
    for i in 0..columns {
        let from = i * buckets / columns.max(1);
        let to = ((i + 1) * buckets / columns.max(1)).max(from + 1);
        let (lo, hi) = wf.range(from, to);
        let x = rect.min.x + i as f32;
        let top = mid - hi.clamp(-1.0, 1.0) * half;
        let bot = mid - lo.clamp(-1.0, 1.0) * half;
        painter.line_segment(
            [Pos2::new(x, top), Pos2::new(x, bot.max(top + 1.0))],
            Stroke::new(1.0, color),
        );
    }
}

/// Removed ranges are shaded out so the shape of the edit is visible at a
/// glance, rather than having to be inferred from a list of segments.
fn dim_cuts(app: &App, ui: &mut egui::Ui, rect: Rect, x_of: &impl Fn(u64) -> f32) {
    let Some(project) = &app.project else { return };
    let painter = ui.painter_at(rect);
    let fps = project.project_fps;
    let segs = project.normalised_segments();
    let total = project.source_frames().0;
    let mut cursor = 0u64;
    let shade = Color32::from_rgba_unmultiplied(10, 10, 12, 190);
    for s in &segs {
        let (start, end) = s.frame_span(fps);
        if start.0 > cursor {
            let r = Rect::from_min_max(
                Pos2::new(x_of(cursor), rect.min.y),
                Pos2::new(x_of(start.0), rect.max.y),
            );
            painter.rect_filled(r, 0.0, shade);
        }
        cursor = end.0;
    }
    if cursor < total {
        let r = Rect::from_min_max(
            Pos2::new(x_of(cursor), rect.min.y),
            Pos2::new(x_of(total), rect.max.y),
        );
        painter.rect_filled(r, 0.0, shade);
    }
}

fn draw_segments(app: &App, ui: &mut egui::Ui, rect: Rect, x_of: &impl Fn(u64) -> f32) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_rgb(22, 23, 26));
    let Some(project) = &app.project else { return };
    let fps = project.project_fps;
    for s in project.normalised_segments() {
        let (start, end) = s.frame_span(fps);
        let r = Rect::from_min_max(
            Pos2::new(x_of(start.0), rect.min.y + 2.0),
            Pos2::new(x_of(end.0), rect.max.y - 2.0),
        );
        painter.rect_filled(r, 2.0, Color32::from_rgb(70, 130, 90));
        painter.rect_stroke(
            r,
            2.0,
            Stroke::new(1.0, Color32::from_rgb(120, 200, 140)),
            StrokeKind::Inside,
        );
    }
}

fn draw_selection(app: &App, ui: &mut egui::Ui, rect: Rect, x_of: &impl Fn(u64) -> f32) {
    let Some((a, b)) = app.selection else { return };
    if b.0 <= a.0 {
        return;
    }
    let painter = ui.painter_at(rect);
    let r = Rect::from_min_max(
        Pos2::new(x_of(a.0), rect.min.y),
        Pos2::new(x_of(b.0), rect.max.y),
    );
    painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(255, 200, 80, 40));
    painter.rect_stroke(
        r,
        0.0,
        Stroke::new(1.0, Color32::from_rgb(255, 200, 80)),
        StrokeKind::Inside,
    );
}

fn draw_ruler(app: &App, ui: &mut egui::Ui, rect: Rect, x_of: &impl Fn(u64) -> f32) {
    let painter = ui.painter_at(rect);
    let Some(project) = &app.project else { return };
    let fps = project.project_fps;
    let total_secs = project.source.duration.as_f64().max(0.001);
    // Aim for a tick roughly every 90 px.
    let target = (total_secs / (rect.width() / 90.0) as f64).max(0.1);
    let step = [
        0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ]
    .into_iter()
    .find(|s| *s >= target)
    .unwrap_or(600.0);
    let mut t = 0.0;
    while t <= total_secs {
        let frame = FrameIdx::from_time(RationalTime::from_seconds_f64(t), fps);
        let x = x_of(frame.0);
        painter.line_segment(
            [Pos2::new(x, rect.min.y), Pos2::new(x, rect.min.y + 4.0)],
            Stroke::new(1.0, Color32::from_gray(70)),
        );
        painter.text(
            Pos2::new(x + 3.0, rect.min.y + 1.0),
            egui::Align2::LEFT_TOP,
            RationalTime::from_seconds_f64(t).to_display(),
            egui::FontId::monospace(9.0),
            Color32::from_gray(110),
        );
        t += step;
    }
}

fn draw_playhead(app: &App, ui: &mut egui::Ui, rect: Rect, x_of: &impl Fn(u64) -> f32) {
    let painter = ui.painter_at(rect);
    let x = x_of(app.playhead.0);
    painter.line_segment(
        [Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)],
        Stroke::new(1.5, Color32::from_rgb(255, 90, 90)),
    );
    painter.circle_filled(
        Pos2::new(x, rect.min.y + 3.0),
        4.0,
        Color32::from_rgb(255, 90, 90),
    );
}
