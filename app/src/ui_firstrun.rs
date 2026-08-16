//! First-run ffmpeg setup.
//!
//! Since a download is the first thing every user experiences on every
//! platform, it is a real screen rather than a silent blocking call — and it
//! always offers a way out for offline users and corporate networks.

use crate::app::{App, Boot};
use crate::bootstrap;
use crate::ffmpeg::{self, Origin};
use crate::jobs::Msg;
use egui::{Color32, RichText};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let pinned = bootstrap::has_pinned_build();
    ui.vertical_centered(|ui| {
        ui.add_space(48.0);
        ui.label(RichText::new("Clip Editor").size(26.0).strong());
        ui.add_space(6.0);
        ui.label(
            RichText::new(if pinned {
                "One-time setup: the video engine has to be downloaded."
            } else {
                "Clip Editor needs ffmpeg, and has no pinned build for this platform."
            })
            .color(Color32::from_gray(170)),
        );
        ui.add_space(20.0);

        let total_mb = bootstrap::total_download_bytes() as f64 / 1_000_000.0;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_max_width(520.0);
            if pinned {
                ui.label(
                    RichText::new(format!(
                        "{}  ·  {total_mb:.0} MB",
                        bootstrap::PINNED_VERSION
                    ))
                    .monospace()
                    .size(11.0),
                );
                ui.label(
                    RichText::new(
                        "ffmpeg does all decoding and encoding. It is downloaded once, \
                         verified against a pinned checksum, and stored in this app's data folder.",
                    )
                    .size(11.0)
                    .color(Color32::from_gray(150)),
                );
            } else {
                ui.label(
                    RichText::new(
                        "ffmpeg does all decoding and encoding. Install it yourself — \
                         `brew install ffmpeg` — and point the app at it below. Anything \
                         reasonably recent works.",
                    )
                    .size(11.0)
                    .color(Color32::from_gray(150)),
                );
            }
            ui.label(
                RichText::new(format!("Source: {}", bootstrap::SOURCE_URL))
                    .size(10.0)
                    .color(Color32::from_gray(120)),
            );
        });

        ui.add_space(16.0);

        match &app.boot {
            Boot::Ready => {}
            Boot::Missing => {
                if pinned && ui.button("Download and install").clicked() {
                    start_download(app, ui.ctx());
                }
                ui.add_space(8.0);
                escape_hatch(app, ui);
            }
            Boot::Downloading {
                done,
                total,
                cancel,
            } => {
                let fraction = if *total > 0 {
                    *done as f32 / *total as f32
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(360.0)
                        .text(format!(
                            "{:.0} / {:.0} MB",
                            *done as f64 / 1e6,
                            *total as f64 / 1e6
                        )),
                );
                let cancel = cancel.clone();
                if ui.button("Cancel").clicked() {
                    cancel.cancel();
                    app.boot = Boot::Missing;
                }
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
            Boot::Failed(err) => {
                ui.label(
                    RichText::new(err.clone())
                        .color(Color32::from_rgb(240, 140, 140))
                        .size(11.0),
                );
                ui.add_space(8.0);
                if pinned {
                    ui.horizontal(|ui| {
                        if ui.button("Retry").clicked() {
                            start_download(app, ui.ctx());
                        }
                    });
                }
                ui.add_space(8.0);
                escape_hatch(app, ui);
            }
        }
    });
}

/// Offline users, corporate networks, and Linux users who already have ffmpeg
/// and would rather not keep a second copy.
fn escape_hatch(app: &mut App, ui: &mut egui::Ui) {
    ui.label(
        RichText::new("Already have ffmpeg?")
            .size(11.0)
            .color(Color32::from_gray(150)),
    );
    ui.horizontal(|ui| {
        if ui.button("Use ffmpeg from PATH").clicked() {
            match ffmpeg::find_on_path() {
                Some(tools) => adopt(app, tools),
                None => {
                    app.boot = Boot::Failed("no ffmpeg and ffprobe found on PATH".into());
                }
            }
        }
        if ui.button("Choose folder…").clicked() {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                match ffmpeg::tools_in_dir(&dir, Origin::System) {
                    Some(tools) => adopt(app, tools),
                    None => {
                        app.boot = Boot::Failed(format!(
                            "{} does not contain both ffmpeg and ffprobe",
                            dir.display()
                        ))
                    }
                }
            }
        }
    });
}

fn adopt(app: &mut App, tools: ffmpeg::Tools) {
    match tools.verify() {
        Ok(_) => {
            let _ = app.tx.send(Msg::BootstrapDone(Ok(tools)));
        }
        Err(e) => app.boot = Boot::Failed(e.to_string()),
    }
}

fn start_download(app: &mut App, ctx: &egui::Context) {
    let cancel = crate::ffmpeg::Cancel::new();
    app.boot = Boot::Downloading {
        done: 0,
        total: bootstrap::total_download_bytes(),
        cancel: cancel.clone(),
    };
    let tx = app.tx.clone();
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("bootstrap".into())
        .spawn(move || {
            let tx2 = tx.clone();
            let ctx2 = ctx.clone();
            let result = bootstrap::install(
                move |p| {
                    let _ = tx2.send(Msg::BootstrapProgress {
                        done: p.done_bytes,
                        total: p.total_bytes,
                    });
                    ctx2.request_repaint();
                },
                || !cancel.is_cancelled(),
            );
            let _ = tx.send(Msg::BootstrapDone(result.map_err(|e| format!("{e:#}"))));
            ctx.request_repaint();
        })
        .expect("spawning bootstrap worker");
}
