//! Window setup and the root layout: toolbar, timeline, sidebar, preview.

use crate::app::{App, Boot};
use crate::glyphs;
use crate::version;
use crate::{cache, keys, ui_export, ui_firstrun, ui_preview, ui_sidebar, ui_timeline};

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title(version::window_title())
            .with_drag_and_drop(true),
        ..Default::default()
    };
    // A path on the command line opens straight away — handy for "open with"
    // integration and for scripted runs.
    let initial = std::env::args().nth(1).map(std::path::PathBuf::from);
    eframe::run_native(
        &version::window_title(),
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc);
            if let Some(path) = initial {
                app.open_path(path);
            }
            Ok(Box::new(app))
        }),
    )
}

/// The whole window, in one function so it can be driven from a test as well
/// as from eframe.
pub fn root_ui(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    app.drain_messages(&ctx);

    if !matches!(app.boot, Boot::Ready) {
        egui::CentralPanel::default().show(ui, |ui| ui_firstrun::show(app, ui));
        return;
    }

    keys::handle(app, &ctx);
    keys::tick_shuttle(app, &ctx);
    app.autosave_tick();
    app.sync_window_title(&ctx);
    app.follow_playback(&ctx);
    accept_dropped_file(app, &ctx);

    egui::Panel::top("toolbar").show(ui, |ui| toolbar(app, ui));
    // Bottom panels stack outwards-in, so the timeline goes first and the
    // transport lands between it and the preview — next to the picture it
    // controls, which is where the hand expects it.
    egui::Panel::bottom("timeline")
        .exact_size(ui_timeline::height())
        .show(ui, |ui| ui_timeline::show(app, ui));
    egui::Panel::bottom("transport").show(ui, |ui| transport(app, ui));
    egui::Panel::right("properties")
        .default_size(300.0)
        .size_range(240.0..=440.0)
        .show(ui, |ui| ui_sidebar::show(app, ui));
    egui::CentralPanel::default().show(ui, |ui| ui_preview::show(app, ui));

    ui_export::overlays(app, &ctx);
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        root_ui(self, ui);
    }

    fn on_exit(&mut self) {
        // Settle the recovery snapshot before tearing anything down: unsaved
        // work is written out, saved work clears it.
        self.finish_session();
        // Kill anything still running and take the intermediates with it.
        self.cancel_import();
        self.cancel_export();
        self.stop_playback();
        if let Some(frames) = &self.frames {
            frames.shutdown();
        }
        cache::cleanup();
    }
}

/// Transport and range editing, directly under the picture they act on.
fn transport(app: &mut App, ui: &mut egui::Ui) {
    let enabled = app.project.is_some();
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        // Centre the cluster under the preview rather than pinning it left.
        let buttons = 430.0;
        ui.add_space(((ui.available_width() - buttons) / 2.0).max(0.0));

        ui.add_enabled_ui(enabled, |ui| {
            if ui
                .button(glyphs::GO_START)
                .on_hover_text("Start of the edit (Home)")
                .clicked()
            {
                app.pause();
                let fps = app.fps();
                let first = app
                    .project
                    .as_ref()
                    .and_then(|p| p.normalised_segments().first().map(|s| s.start_frame(fps)))
                    .unwrap_or(clip_core::FrameIdx(0));
                app.seek_source(first);
            }
            if ui
                .button(glyphs::STEP_BACK)
                .on_hover_text("Step back (←)")
                .clicked()
            {
                app.step(-1);
            }

            let playing = app.player.is_playing() || app.shuttle != 0;
            if ui
                .add(egui::Button::new(
                    egui::RichText::new(if playing { glyphs::PAUSE } else { glyphs::PLAY })
                        .size(15.0),
                ))
                .on_hover_text("Play / pause (Space)")
                .clicked()
            {
                app.toggle_play();
            }

            if ui
                .button(glyphs::STEP_FORWARD)
                .on_hover_text("Step forward (→)")
                .clicked()
            {
                app.step(1);
            }
            if ui
                .button(glyphs::GO_END)
                .on_hover_text("End of the edit (End)")
                .clicked()
            {
                app.pause();
                let fps = app.fps();
                let last = app
                    .project
                    .as_ref()
                    .and_then(|p| p.normalised_segments().last().map(|s| s.end_frame(fps)))
                    .map(|f| f.saturating_sub(1))
                    .unwrap_or(clip_core::FrameIdx(0));
                app.seek_source(last);
            }

            ui.separator();

            if ui
                .button("Set in")
                .on_hover_text("Trim the start of this segment to the playhead (I)")
                .clicked()
            {
                keys::set_in(app);
            }
            if ui
                .button("Set out")
                .on_hover_text("Trim the end of this segment to the playhead (O)")
                .clicked()
            {
                keys::set_out(app);
            }
            if ui
                .button("Delete range")
                .on_hover_text(
                    "Removes the marked range, or from the playhead to the end of \
                     the current segment (X)",
                )
                .clicked()
            {
                keys::delete_range(app);
            }
        });

        // Position readout hugs the right edge of the strip.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let fps = app.fps();
            let position = app.playhead.to_time(fps);
            ui.label(
                egui::RichText::new(format!(
                    "{}  ·  frame {}",
                    position.to_display(),
                    app.playhead.0
                ))
                .monospace()
                .size(11.0),
            );
        });
    });
    ui.add_space(2.0);
}

fn toolbar(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        if ui.button("Open…").clicked() {
            app.open_dialog();
        }
        if ui
            .add_enabled(app.project.is_some(), egui::Button::new("Save"))
            .on_hover_text("Save the project (Ctrl+S)")
            .clicked()
        {
            app.save_project();
        }
        ui.separator();

        if ui
            .add_enabled(app.undo.can_undo(), egui::Button::new(glyphs::UNDO))
            .on_hover_text("Undo (Ctrl+Z)")
            .clicked()
        {
            app.undo();
        }
        if ui
            .add_enabled(app.undo.can_redo(), egui::Button::new(glyphs::REDO))
            .on_hover_text("Redo (Ctrl+Shift+Z)")
            .clicked()
        {
            app.redo();
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // The playhead position lives on the transport strip now; what
            // belongs up here is the state of the edit as a whole.
            let kept = app
                .project
                .as_ref()
                .map(|p| p.output_duration())
                .unwrap_or(clip_core::RationalTime::ZERO);
            if app.project.is_some() {
                ui.label(
                    egui::RichText::new(format!("keeping {}", kept.to_display()))
                        .monospace()
                        .size(11.0),
                );
            }
            if !app.status.is_empty() {
                ui.label(
                    egui::RichText::new(&app.status)
                        .size(11.0)
                        .color(egui::Color32::from_gray(150)),
                );
                ui.spinner();
            }
        });
    });
}

fn accept_dropped_file(app: &mut App, ctx: &egui::Context) {
    let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .map(|f| f.path().to_path_buf())
            .collect()
    });
    if let Some(path) = dropped.into_iter().next() {
        app.open_path(path);
    }
}
