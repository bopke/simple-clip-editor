//! The properties sidebar: source info, crop, audio, output settings.

use crate::app::App;
use clip_core::audio_math::{self, MUTE_DB};
use clip_core::crop;
use clip_core::model::ASPECT_PRESETS;
use clip_core::{AspectMode, AudioSourceSelection, OutputPreset, Quality, QualityBucket, Rational};
use egui::{Color32, RichText};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        source_section(app, ui);
        ui.separator();
        crop_section(app, ui);
        ui.separator();
        audio_section(app, ui);
        ui.separator();
        output_section(app, ui);
        ui.separator();
        shortcuts(ui);
    });
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(
        RichText::new(text)
            .size(11.0)
            .color(Color32::from_gray(150))
            .strong(),
    );
    ui.add_space(2.0);
}

fn source_section(app: &mut App, ui: &mut egui::Ui) {
    heading(ui, "SOURCE");
    let Some(project) = &app.project else {
        ui.label(RichText::new("No file open").color(Color32::from_gray(120)));
        if ui.button("Open video…").clicked() {
            app.open_dialog();
        }
        return;
    };
    let src = &project.source;
    ui.label(
        RichText::new(
            src.path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        )
        .strong(),
    );
    let (dw, dh) = src.display_size();
    ui.label(
        RichText::new(format!(
            "{dw}×{dh}  ·  {}  ·  {}",
            src.avg_frame_rate,
            src.duration.to_display()
        ))
        .size(11.0)
        .color(Color32::from_gray(140)),
    );
    if src.is_vfr() {
        ui.label(
            RichText::new("Variable frame rate — normalised on import")
                .size(11.0)
                .color(Color32::from_rgb(220, 190, 120)),
        );
    }
    if src.is_hdr {
        ui.label(
            RichText::new("HDR source — tone-mapped to SDR")
                .size(11.0)
                .color(Color32::from_rgb(220, 190, 120)),
        );
    }
    if src.rotation != 0 {
        ui.label(
            RichText::new(format!("Rotation {}° — normalised", src.rotation))
                .size(11.0)
                .color(Color32::from_gray(140)),
        );
    }

    let current = project.project_fps;
    ui.horizontal(|ui| {
        ui.label("Timeline fps");
        let mut chosen = current;
        egui::ComboBox::from_id_salt("project-fps")
            .selected_text(current.to_display_fps())
            .show_ui(ui, |ui| {
                for r in common_rates(current) {
                    ui.selectable_value(&mut chosen, r, r.to_display_fps());
                }
            });
        if chosen != current {
            app.set_project_fps(chosen);
        }
    });

    if app.import_progress < 1.0 && app.import_job.is_some() {
        ui.add(
            egui::ProgressBar::new(app.import_progress)
                .desired_height(6.0)
                .text("proxy"),
        );
    }
}

fn common_rates(current: Rational) -> Vec<Rational> {
    let mut v = vec![
        Rational::new(24000, 1001),
        Rational::from_int(24),
        Rational::from_int(25),
        Rational::new(30000, 1001),
        Rational::from_int(30),
        Rational::from_int(50),
        Rational::new(60000, 1001),
        Rational::from_int(60),
        Rational::from_int(120),
    ];
    if !v.contains(&current) {
        v.push(current);
    }
    v
}

fn crop_section(app: &mut App, ui: &mut egui::Ui) {
    heading(ui, "CROP");
    let Some(project) = app.project.as_mut() else {
        return;
    };
    let bounds = project.source.display_size();
    let mut crop_tool = app.crop_tool;
    ui.checkbox(&mut crop_tool, "Crop tool  (C)");
    app.crop_tool = crop_tool;

    let mut mode = project.aspect_mode;
    let mut apply: Option<Rational> = None;
    ui.horizontal_wrapped(|ui| {
        if ui
            .selectable_label(matches!(mode, AspectMode::Free), "Free")
            .clicked()
        {
            mode = AspectMode::Free;
        }
        if ui.selectable_label(false, "Original").clicked() {
            mode = AspectMode::Free;
            apply = Some(Rational::new(bounds.0 as i64, bounds.1.max(1) as i64));
        }
        for (name, ratio) in ASPECT_PRESETS {
            let selected = mode.locked_ratio() == Some(*ratio);
            if ui.selectable_label(selected, *name).clicked() {
                mode = AspectMode::Locked(*ratio);
                apply = Some(*ratio);
            }
        }
    });

    // Pad instead of crop — for gameplay footage the subject often does not
    // survive being cropped to 9:16.
    let mut pad = matches!(mode, AspectMode::PadToRatio(_));
    let pad_ratio = mode.locked_ratio().unwrap_or(Rational::new(9, 16));
    if ui
        .checkbox(&mut pad, "Pad to ratio instead of cropping")
        .changed()
    {
        mode = if pad {
            AspectMode::PadToRatio(pad_ratio)
        } else {
            AspectMode::Locked(pad_ratio)
        };
    }
    project.aspect_mode = mode;

    if let Some(ratio) = apply {
        let current = project.crop.unwrap_or_else(|| crop::full_frame(bounds));
        project.crop = Some(crop::apply_ratio(current, ratio, bounds));
        // Picking a ratio is a request to crop, so show the handles rather
        // than making the user find the checkbox to see what they just did.
        app.crop_tool = true;
    }

    if let Some(c) = project.crop {
        ui.label(
            RichText::new(crop::describe(c))
                .monospace()
                .size(11.0)
                .color(Color32::from_gray(190)),
        );
        if ui.button("Reset crop").clicked() {
            project.crop = None;
        }
    } else {
        ui.label(
            RichText::new("Full frame")
                .size(11.0)
                .color(Color32::from_gray(140)),
        );
    }
    let (ow, oh) = project.output_size();
    ui.label(
        RichText::new(format!("Output {ow}×{oh}"))
            .size(11.0)
            .color(Color32::from_gray(160)),
    );
}

fn audio_section(app: &mut App, ui: &mut egui::Ui) {
    heading(ui, "AUDIO");
    let mut changed = false;
    let mut remove: Option<usize> = None;
    let mut add = false;
    {
        let Some(project) = app.project.as_mut() else {
            return;
        };
        let streams = project.source.audio_streams.clone();
        if streams.is_empty() {
            ui.label(
                RichText::new("Source has no audio")
                    .size(11.0)
                    .color(Color32::from_gray(140)),
            );
        } else {
            let sel = &mut project.original_audio.source;
            let label = match sel {
                AudioSourceSelection::None => "Muted".to_string(),
                AudioSourceSelection::MixAll => "All streams".to_string(),
                AudioSourceSelection::Stream(i) => streams
                    .get(*i)
                    .map(|s| s.label())
                    .unwrap_or_else(|| format!("Stream {i}")),
            };
            egui::ComboBox::from_id_salt("orig-audio")
                .selected_text(label)
                .width(200.0)
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(sel, AudioSourceSelection::None, "Muted")
                        .changed();
                    for (i, s) in streams.iter().enumerate() {
                        changed |= ui
                            .selectable_value(sel, AudioSourceSelection::Stream(i), s.label())
                            .changed();
                    }
                    if streams.len() > 1 {
                        // OBS records desktop and mic separately; defaulting to
                        // one of them silently drops the other.
                        changed |= ui
                            .selectable_value(
                                sel,
                                AudioSourceSelection::MixAll,
                                "All streams (desktop + mic)",
                            )
                            .changed();
                    }
                });
            changed |= gain_slider(ui, "Original", &mut project.original_audio.gain_db);
        }

        ui.add_space(6.0);
        for (i, track) in project.added_tracks.iter_mut().enumerate() {
            ui.push_id(i, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(track.name()).size(11.0).strong());
                    if ui.small_button(crate::glyphs::REMOVE).clicked() {
                        remove = Some(i);
                    }
                });
                changed |= gain_slider(ui, "Gain", &mut track.gain_db);
                let mut offset = track.offset.as_f64() as f32;
                if ui
                    .add(
                        egui::Slider::new(&mut offset, -30.0..=120.0)
                            .text("offset s")
                            .fixed_decimals(2),
                    )
                    .changed()
                {
                    // Negative offsets are legal: the track starts before the
                    // clip does, and export handles it with an input seek.
                    track.offset = clip_core::RationalTime::from_seconds_f64(offset as f64);
                    changed = true;
                }
                changed |= ui
                    .checkbox(&mut track.fade_out, "Fade out at end")
                    .changed();
            });
        }
        if ui.button("Add audio track…").clicked() {
            add = true;
        }
    }
    if let Some(i) = remove {
        app.remove_audio_track(i);
    } else if add {
        app.add_audio_track();
    } else if changed {
        app.refresh_mix();
    }
}

fn gain_slider(ui: &mut egui::Ui, label: &str, db: &mut f32) -> bool {
    let mut value = if db.is_infinite() { MUTE_DB } else { *db };
    let response = ui.add(
        egui::Slider::new(&mut value, MUTE_DB..=12.0)
            .text(label)
            .custom_formatter(|v, _| audio_math::format_db(v as f32)),
    );
    if response.changed() {
        *db = value;
        return true;
    }
    false
}

fn output_section(app: &mut App, ui: &mut egui::Ui) {
    heading(ui, "OUTPUT");
    let hw_available = !app.hw_encoders.is_empty();
    let hw_label = app
        .hw_encoders
        .first()
        .map(|e| e.label().to_string())
        .unwrap_or_default();
    let mut preset_changed = false;
    {
        let Some(project) = app.project.as_mut() else {
            return;
        };
        // Validated bundles, not an independent container/codec matrix.
        egui::ComboBox::from_id_salt("preset")
            .selected_text(project.output.preset.label())
            .width(200.0)
            .show_ui(ui, |ui| {
                for p in OutputPreset::ALL {
                    preset_changed |= ui
                        .selectable_value(&mut project.output.preset, *p, p.label())
                        .changed();
                }
            });

        if project.output.preset.has_video() {
            let mut long_edge = project.output.long_edge.unwrap_or(0);
            ui.horizontal(|ui| {
                ui.label("Long edge");
                egui::ComboBox::from_id_salt("long-edge")
                    .selected_text(if long_edge == 0 {
                        "Native".to_string()
                    } else {
                        format!("{long_edge}px")
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut long_edge, 0, "Native");
                        for v in [720u32, 1080, 1440, 1920, 2160] {
                            ui.selectable_value(&mut long_edge, v, format!("{v}px"));
                        }
                    });
            });
            project.output.long_edge = (long_edge > 0).then_some(long_edge);
        }

        if project.output.preset.uses_crf() {
            let preset = project.output.preset;
            let mut crf = project.output.quality.crf_for(preset);
            if ui
                .add(egui::Slider::new(&mut crf, 12..=40).text("CRF"))
                .changed()
            {
                project.output.quality = Quality::Crf(crf);
            }
            ui.horizontal(|ui| {
                for (label, bucket) in [
                    ("Small", QualityBucket::Small),
                    ("Balanced", QualityBucket::Balanced),
                    ("High", QualityBucket::High),
                ] {
                    if ui.small_button(label).clicked() {
                        project.output.quality = Quality::Bucket(bucket);
                    }
                }
            });
        }

        if project.output.preset.supports_hardware() && hw_available {
            ui.checkbox(
                &mut project.output.use_hardware_encoder,
                format!("Fast export ({hw_label})"),
            );
        }
    }

    if preset_changed {
        app.output_path = app.project.as_ref().map(crate::export::suggested_output);
    }

    ui.add_space(4.0);
    let path_text = app
        .output_path
        .as_ref()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
        .unwrap_or_else(|| "—".into());
    ui.horizontal(|ui| {
        ui.label(RichText::new(path_text).size(11.0).monospace());
        if ui.small_button("…").clicked() {
            pick_output(app);
        }
    });

    if app.export_job.is_some() {
        ui.add(
            egui::ProgressBar::new(app.export_progress)
                .desired_height(10.0)
                .text(app.export_label.clone()),
        );
        if ui.button("Cancel export").clicked() {
            app.cancel_export();
        }
    } else {
        let enabled = app.project.is_some() && app.tools.is_some();
        if ui
            .add_enabled(enabled, egui::Button::new("Export  (Ctrl+E)"))
            .clicked()
        {
            let ctx = ui.ctx().clone();
            app.start_export(&ctx);
        }
    }

    match &app.export_result {
        Some(Ok(path)) => {
            ui.label(
                RichText::new(format!("Exported {}", path.display()))
                    .size(11.0)
                    .color(Color32::from_rgb(140, 220, 140)),
            );
            if ui.small_button("Show in folder").clicked() {
                reveal(path);
            }
        }
        Some(Err(e)) => {
            ui.label(
                RichText::new(format!("Export failed: {e}"))
                    .size(11.0)
                    .color(Color32::from_rgb(240, 140, 140)),
            );
        }
        None => {}
    }
}

fn pick_output(app: &mut App) {
    let Some(project) = &app.project else { return };
    let ext = project.output.preset.extension();
    let start = app
        .output_path
        .clone()
        .unwrap_or_else(|| crate::export::suggested_output(project));
    let dialog = rfd::FileDialog::new()
        .add_filter(ext, &[ext])
        .set_file_name(
            start
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default(),
        );
    let dialog = match start.parent() {
        Some(dir) => dialog.set_directory(dir),
        None => dialog,
    };
    if let Some(path) = dialog.save_file() {
        app.output_path = Some(path);
    }
}

fn reveal(path: &std::path::Path) {
    let dir = path.parent().unwrap_or(path);
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

fn shortcuts(ui: &mut egui::Ui) {
    egui::CollapsingHeader::new("Controls")
        .default_open(false)
        .show(ui, |ui| {
            egui::Grid::new("shortcuts").num_columns(2).show(ui, |ui| {
                for (key, action) in crate::keys::HELP {
                    ui.label(RichText::new(*key).monospace().size(10.0));
                    ui.label(RichText::new(*action).size(10.0));
                    ui.end_row();
                }
            });
        });
}
