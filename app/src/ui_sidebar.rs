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
        if ui.button("Open video or project…").clicked() {
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
    // The probe has always read these and nothing has ever shown them. They
    // are what a slow import or an odd colour cast gets blamed on, so the
    // answer belongs where someone can see it rather than in a struct.
    let format = [src.video_codec.as_deref(), src.container.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("  ·  ");
    if !format.is_empty() {
        ui.label(
            RichText::new(format)
                .size(11.0)
                .color(Color32::from_gray(120)),
        );
    }
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
    // Read out what is being drawn, draw against copies, then commit through
    // `App::edit`. Holding `&mut Project` across the widgets is what made every
    // control here mutate the project behind the app's back.
    let Some(project) = app.project.as_ref() else {
        return;
    };
    let bounds = project.source.display_size();
    let was = (project.aspect_mode, project.crop);
    let mut crop_tool = app.crop_tool;
    ui.checkbox(&mut crop_tool, "Crop tool  (C)");
    app.crop_tool = crop_tool;

    let mut mode = was.0;
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
    let mut crop = was.1;
    if let Some(ratio) = apply {
        let current = crop.unwrap_or_else(|| crop::full_frame(bounds));
        crop = Some(crop::apply_ratio(current, ratio, bounds));
        // Picking a ratio is a request to crop, so show the handles rather
        // than making the user find the checkbox to see what they just did.
        app.crop_tool = true;
    }

    if let Some(c) = crop {
        ui.label(
            RichText::new(crop::describe(c))
                .monospace()
                .size(11.0)
                .color(Color32::from_gray(190)),
        );
        if ui.button("Reset crop").clicked() {
            crop = None;
        }
    } else {
        ui.label(
            RichText::new("Full frame")
                .size(11.0)
                .color(Color32::from_gray(140)),
        );
    }

    if (mode, crop) != was {
        app.set_aspect_and_crop(mode, crop);
    }

    // Read again: the label describes the project after the edit, not before.
    if let Some((ow, oh)) = app.project.as_ref().map(|p| p.output_size()) {
        ui.label(
            RichText::new(format!("Output {ow}×{oh}"))
                .size(11.0)
                .color(Color32::from_gray(160)),
        );
    }
}

/// One change the audio section asks for. Collected while drawing and applied
/// afterwards, because a widget needs `&mut` to the value it is showing and the
/// project cannot be borrowed and edited at the same time.
enum AudioChange {
    Source(AudioSourceSelection),
    OriginalGain(f32),
    TrackGain(usize, f32),
    TrackOffset(usize, f32),
    TrackTrim(usize, f32),
    TrackFade(usize, bool),
}

fn audio_section(app: &mut App, ui: &mut egui::Ui) {
    heading(ui, "AUDIO");
    let mut change: Option<AudioChange> = None;
    let mut remove: Option<usize> = None;
    let mut add = false;
    {
        let Some(project) = app.project.as_ref() else {
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
            let mut sel = project.original_audio.source;
            let label = match sel {
                AudioSourceSelection::None => "Muted".to_string(),
                AudioSourceSelection::MixAll => "All streams".to_string(),
                AudioSourceSelection::Stream(i) => streams
                    .get(i)
                    .map(|s| s.label())
                    .unwrap_or_else(|| format!("Stream {i}")),
            };
            egui::ComboBox::from_id_salt("orig-audio")
                .selected_text(label)
                .width(200.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut sel, AudioSourceSelection::None, "Muted");
                    for (i, s) in streams.iter().enumerate() {
                        ui.selectable_value(&mut sel, AudioSourceSelection::Stream(i), s.label());
                    }
                    if streams.len() > 1 {
                        // OBS records desktop and mic separately; defaulting to
                        // one of them silently drops the other.
                        ui.selectable_value(
                            &mut sel,
                            AudioSourceSelection::MixAll,
                            "All streams (desktop + mic)",
                        );
                    }
                });
            if sel != project.original_audio.source {
                change = Some(AudioChange::Source(sel));
            }
            let mut gain = project.original_audio.gain_db;
            if gain_slider(ui, "Original", &mut gain) {
                change = Some(AudioChange::OriginalGain(gain));
            }
        }

        ui.add_space(6.0);
        for (i, track) in project.added_tracks.iter().enumerate() {
            ui.push_id(i, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(track.name()).size(11.0).strong());
                    if ui.small_button(crate::glyphs::REMOVE).clicked() {
                        remove = Some(i);
                    }
                });
                let mut gain = track.gain_db;
                if gain_slider(ui, "Gain", &mut gain) {
                    change = Some(AudioChange::TrackGain(i, gain));
                }
                let mut offset = track.offset.as_f64() as f32;
                if ui
                    .add(
                        egui::Slider::new(&mut offset, -30.0..=120.0)
                            .text("offset s")
                            .fixed_decimals(2),
                    )
                    .changed()
                {
                    change = Some(AudioChange::TrackOffset(i, offset));
                }
                // Where in the file to start from. The model and the export
                // have always honoured this; nothing ever offered it.
                let mut trim = track.trim_in.map(|t| t.as_f64() as f32).unwrap_or(0.0);
                let longest = track
                    .duration
                    .map(|d| d.as_f64() as f32)
                    .unwrap_or(120.0)
                    .max(1.0);
                if ui
                    .add(
                        egui::Slider::new(&mut trim, 0.0..=longest)
                            .text("start at s")
                            .fixed_decimals(2),
                    )
                    .changed()
                {
                    change = Some(AudioChange::TrackTrim(i, trim));
                }
                let mut fade = track.fade_out;
                if ui.checkbox(&mut fade, "Fade out at end").changed() {
                    change = Some(AudioChange::TrackFade(i, fade));
                }
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
    } else if let Some(change) = change {
        apply_audio_change(app, change);
    }
}

fn apply_audio_change(app: &mut App, change: AudioChange) {
    match change {
        AudioChange::Source(sel) => app.set_original_audio_source(sel),
        AudioChange::TrackFade(i, on) => app.set_track_fade_out(i, on),
        AudioChange::OriginalGain(db) => app.set_original_gain(db),
        AudioChange::TrackGain(i, db) => app.set_track_gain(i, db),
        AudioChange::TrackOffset(i, secs) => app.set_track_offset(i, secs),
        AudioChange::TrackTrim(i, secs) => app.set_track_trim_in(i, secs),
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
    let mut crf_change: Option<u32> = None;
    let settings = {
        let Some(project) = app.project.as_ref() else {
            return;
        };
        let was = project.output.clone();
        let mut settings = was.clone();

        // Validated bundles, not an independent container/codec matrix.
        egui::ComboBox::from_id_salt("preset")
            .selected_text(settings.preset.label())
            .width(200.0)
            .show_ui(ui, |ui| {
                for p in OutputPreset::ALL {
                    ui.selectable_value(&mut settings.preset, *p, p.label());
                }
            });

        if settings.preset.has_video() {
            let mut long_edge = settings.long_edge.unwrap_or(0);
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
            settings.long_edge = (long_edge > 0).then_some(long_edge);
        }

        if settings.preset.uses_crf() {
            let preset = settings.preset;
            let mut crf = settings.quality.crf_for(preset);
            // The CRF slider is the one control here that is dragged, so it
            // reports separately and coalesces; everything else is a click.
            if ui
                .add(egui::Slider::new(&mut crf, 12..=40).text("CRF"))
                .changed()
            {
                crf_change = Some(crf);
            }
            ui.horizontal(|ui| {
                for (label, bucket) in [
                    ("Small", QualityBucket::Small),
                    ("Balanced", QualityBucket::Balanced),
                    ("High", QualityBucket::High),
                ] {
                    if ui.small_button(label).clicked() {
                        settings.quality = Quality::Bucket(bucket);
                    }
                }
            });
        }

        if settings.preset.supports_hardware() && hw_available {
            ui.checkbox(
                &mut settings.use_hardware_encoder,
                format!("Fast export ({hw_label})"),
            );
        }
        let preset_changed = was.preset != settings.preset;
        (settings != was).then_some((settings, preset_changed))
    };

    if let Some((settings, preset_changed)) = settings {
        app.set_output_settings(settings);
        if preset_changed {
            // The suggested name carries the preset's extension, so it follows.
            app.output_path = app.project.as_ref().map(crate::export::suggested_output);
        }
    } else if let Some(crf) = crf_change {
        app.set_crf(crf);
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
            app.pick_output();
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
