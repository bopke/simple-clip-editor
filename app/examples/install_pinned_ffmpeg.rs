//! Runs the real first-run bootstrap and prints where it landed.
//!
//! This exists for CI. The suite otherwise tests against whatever ffmpeg the
//! runner's package manager provides — three different builds, none of them the
//! ones the app actually downloads, with different configure flags and codec
//! availability. `exports_every_preset` passing against `apt install ffmpeg`
//! says nothing about whether ProRes or VP9 work in the shipped app.
//!
//! Stdout is the install directory and nothing else, so a workflow can do
//! `CLIP_EDITOR_FFMPEG=$(cargo run --example install_pinned_ffmpeg)` and point
//! the end-to-end tests at the same binaries a user gets. Progress goes to
//! stderr.

use std::io::Write;

fn main() {
    if !clip_editor::bootstrap::has_pinned_build() {
        eprintln!("no pinned ffmpeg build for this platform");
        std::process::exit(2);
    }

    // An already-installed copy of the pinned version is the same bytes — the
    // stamp is only written after a complete install of exactly this pin — so a
    // cached one is worth reusing rather than fetching 168 MB again.
    let tools = match clip_editor::bootstrap::installed() {
        Some(tools) => {
            eprintln!(
                "already installed: {}",
                clip_editor::bootstrap::PINNED_VERSION
            );
            tools
        }
        None => {
            let mut last_percent = u64::MAX;
            let result = clip_editor::bootstrap::install(
                |p| {
                    let percent = p.done_bytes.saturating_mul(100) / p.total_bytes.max(1);
                    // One line per 10%, so a CI log stays readable.
                    if percent / 10 != last_percent / 10 {
                        last_percent = percent;
                        eprintln!("downloading… {percent}%");
                        let _ = std::io::stderr().flush();
                    }
                },
                || true,
            );
            match result {
                Ok(tools) => tools,
                Err(e) => {
                    eprintln!("bootstrap failed: {e:#}");
                    std::process::exit(1);
                }
            }
        }
    };

    match tools.verify() {
        Ok(banner) => eprintln!("{banner}"),
        Err(e) => {
            eprintln!("the installed ffmpeg would not run: {e:#}");
            std::process::exit(1);
        }
    }

    let dir = tools
        .ffmpeg
        .parent()
        .expect("the binary is inside the install directory");
    println!("{}", dir.display());
}
