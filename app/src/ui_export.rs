//! Modal overlays: the export dialog, HDR notice, and error toasts.

use crate::app::{App, PendingOpen};
use egui::{Color32, RichText};

pub fn overlays(app: &mut App, ctx: &egui::Context) {
    recovery_notice(app, ctx);
    discard_confirm(app, ctx);
    export_confirm(app, ctx);
    export_dialog(app, ctx);
    hdr_notice(app, ctx);
    errors(app, ctx);
}

/// What the export was about to do quietly: replace a file, or start something
/// the disk cannot hold. Neither is a refusal — both are answerable, and the
/// user may well know better than the estimate does.
fn export_confirm(app: &mut App, ctx: &egui::Context) {
    let Some(checks) = &app.export_confirm else {
        return;
    };
    let path = app
        .output_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let mut go = false;
    let mut cancel = false;
    let mut choose = false;
    // A question, so it blocks: unlike the export dialog behind it, this is
    // momentary and there is nothing useful to do until it is answered.
    egui::Modal::new(egui::Id::new("export-confirm")).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.heading("Before exporting");
        ui.add_space(4.0);

        if let Some(bytes) = checks.overwrite {
            ui.label(format!(
                "A file is already there and will be replaced ({}):",
                crate::disk::human_bytes(bytes)
            ));
            ui.label(RichText::new(&path).monospace().size(10.0));
            ui.add_space(6.0);
        }

        for warning in &checks.tight {
            ui.label(
                RichText::new(format!(
                    "Not enough room for {} on {}: about {} needed, {} free.",
                    warning.what,
                    warning.path.display(),
                    crate::disk::human_bytes(warning.needed),
                    crate::disk::human_bytes(warning.available),
                ))
                .color(Color32::from_rgb(240, 190, 140)),
            );
        }
        if !checks.tight.is_empty() {
            ui.label(
                RichText::new(
                    "The size is an estimate from the source file — the encode may \
                         come out smaller.",
                )
                .size(11.0)
                .color(Color32::from_gray(150)),
            );
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            go = ui.button("Export anyway").clicked();
            choose = ui.button("Choose another file…").clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });

    if go {
        let ctx = ctx.clone();
        app.start_export_confirmed(&ctx);
    } else if choose {
        app.export_confirm = None;
        // A different file is a different set of answers, so ask again against
        // the new one — which may need no warning at all. A cancelled dialog
        // changes nothing and does not put the warning straight back up.
        if app.pick_output() {
            let ctx = ctx.clone();
            app.start_export(&ctx);
        }
    } else if cancel {
        app.export_confirm = None;
    }
}

/// Unsaved work from a session that ended badly. Shown before anything else,
/// because opening a file would otherwise throw away the very thing being
/// offered.
///
/// A real [`egui::Modal`], not a window that merely sits on top. The comment
/// here used to claim it blocked everything behind it while the toolbar and
/// Ctrl+O stayed live — so the one action the notice exists to prevent was
/// still one click away. Keyboard shortcuts are held off separately, in
/// [`crate::keys::handle`], since a backdrop only stops the pointer.
fn recovery_notice(app: &mut App, ctx: &egui::Context) {
    let Some(pending) = &app.pending_recovery else {
        return;
    };
    let label = pending.label();
    let mut restore = false;
    let mut discard = false;
    egui::Modal::new(egui::Id::new("recovery-notice")).show(ctx, |ui| {
        ui.set_max_width(420.0);
        ui.heading("Unsaved work");
        ui.add_space(4.0);
        ui.label(format!(
            "Clip last closed with unsaved changes to {label} — either \
             without saving, or without closing at all."
        ));
        ui.label(
            RichText::new("Restoring opens it as an unsaved edit — save it somewhere to keep it.")
                .size(11.0)
                .color(Color32::from_gray(150)),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            restore = ui.button("Restore").clicked();
            discard = ui.button("Discard").clicked();
        });
    });
    if restore {
        app.restore_recovery();
    } else if discard {
        app.discard_recovery();
    }
}

/// The question nothing used to ask: this open will throw away work that is
/// not saved anywhere.
fn discard_confirm(app: &mut App, ctx: &egui::Context) {
    let Some(intent) = app.confirm_discard.clone() else {
        return;
    };
    let what = match &intent {
        PendingOpen::Dialog => "Opening another file".to_string(),
        PendingOpen::Path(path) => format!(
            "Opening {}",
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string())
        ),
        PendingOpen::Recovery => "Restoring the recovered edit".to_string(),
    };

    let mut save = false;
    let mut discard = false;
    let mut cancel = false;
    egui::Modal::new(egui::Id::new("discard-confirm")).show(ctx, |ui| {
        ui.set_max_width(440.0);
        ui.heading("Unsaved changes");
        ui.add_space(4.0);
        ui.label(format!(
            "{what} closes the edit you have open, and it has changes that are \
             not saved to a file."
        ));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            save = ui.button("Save, then open").clicked();
            discard = ui.button("Discard changes").clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });

    if save {
        app.save_and_open();
    } else if discard {
        app.discard_and_open();
    } else if cancel {
        app.cancel_pending_open();
    }
}

/// A window rather than a modal, deliberately.
///
/// This stays open while the export runs — the progress bar and the cancel
/// button are in here — and blocking the editor behind it for a twenty-minute
/// encode would be worse than anything modality buys. It still owns the
/// *keyboard* while it is up (see [`crate::app::Dialog`]), so Space does not
/// toggle playback and `X` does not delete a range under it; Esc closes it and
/// leaves any running export alone.
fn export_dialog(app: &mut App, ctx: &egui::Context) {
    if !app.show_export {
        return;
    }
    let mut open = true;
    egui::Window::new("Export")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .open(&mut open)
        .show(ctx, |ui| {
            let Some(project) = &app.project else {
                ui.label("Nothing to export.");
                return;
            };
            let (w, h) = project.output_size();
            let segments = project.normalised_segments();
            egui::Grid::new("export-summary")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Preset");
                    ui.label(project.output.preset.label());
                    ui.end_row();
                    ui.label("Resolution");
                    ui.label(format!("{w} × {h}"));
                    ui.end_row();
                    ui.label("Frame rate");
                    ui.label(project.output_fps().to_ffmpeg_arg());
                    ui.end_row();
                    ui.label("Duration");
                    ui.label(project.output_duration().to_display());
                    ui.end_row();
                    ui.label("Segments");
                    ui.label(format!("{}", segments.len()));
                    ui.end_row();
                    ui.label("Audio tracks");
                    ui.label(format!("{}", project.added_tracks.len()));
                    ui.end_row();
                    ui.label("File");
                    ui.label(
                        RichText::new(
                            app.output_path
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                        )
                        .monospace()
                        .size(10.0),
                    );
                    ui.end_row();
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if app.export_job.is_some() {
                    ui.add(
                        egui::ProgressBar::new(app.export_progress)
                            .desired_width(220.0)
                            .text(app.export_label.clone()),
                    );
                    if ui.button("Cancel").clicked() {
                        app.cancel_export();
                    }
                } else if ui.button("Start export").clicked() {
                    let ctx = ui.ctx().clone();
                    app.start_export(&ctx);
                }
            });
            if let Some(result) = &app.export_result {
                match result {
                    Ok(path) => {
                        ui.label(
                            RichText::new(format!("Done: {}", path.display()))
                                .color(Color32::from_rgb(140, 220, 140))
                                .size(11.0),
                        );
                    }
                    Err(e) => {
                        ui.label(
                            RichText::new(e)
                                .color(Color32::from_rgb(240, 140, 140))
                                .size(11.0),
                        );
                    }
                }
            }
        });
    if !open {
        app.show_export = false;
    }
    if app.export_job.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
    }
}

fn hdr_notice(app: &mut App, ctx: &egui::Context) {
    if !app.hdr_warning {
        return;
    }
    let mut open = true;
    egui::Window::new("HDR source")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(
                "This recording is HDR. It will be tone-mapped to SDR (BT.709) on \
                 import and on export, so the preview matches the output.",
            );
            ui.label(
                RichText::new("Tone-mapping is not free: expect import to take longer than usual.")
                    .size(11.0)
                    .color(Color32::from_gray(150)),
            );
            if ui.button("Got it").clicked() {
                app.hdr_warning = false;
            }
        });
    if !open {
        app.hdr_warning = false;
    }
}

fn errors(app: &mut App, ctx: &egui::Context) {
    if app.errors.is_empty() {
        return;
    }
    let mut dismiss = false;
    egui::Window::new("Problem")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_BOTTOM, egui::Vec2::new(0.0, -24.0))
        .show(ctx, |ui| {
            ui.set_max_width(520.0);
            for e in app.errors.recent(4) {
                ui.label(
                    RichText::new(e.label())
                        .size(11.0)
                        .color(Color32::from_rgb(240, 160, 160)),
                );
            }
            if ui.button("Dismiss").clicked() {
                dismiss = true;
            }
        });
    if dismiss {
        app.errors.clear();
    }
}
