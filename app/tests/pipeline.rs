//! End-to-end tests against a real ffmpeg. These are the tests that catch the
//! failure mode golden argument tests cannot: arguments that are well-formed
//! but that ffmpeg rejects, or that silently produce the wrong thing.
//!
//! Skipped (not failed) when no ffmpeg is available, so a checkout without one
//! still has a green test suite.

use clip_core::{
    command, AudioSourceSelection, AudioTrack, CropRect, FrameIdx, OutputPreset, Project, Rational,
    RationalTime, Segment,
};
use clip_editor::export;
use clip_editor::ffmpeg::{Cancel, Tools};
use clip_editor::import;
use clip_editor::jobs::{self, Msg};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tools() -> Option<Tools> {
    clip_editor::ffmpeg::from_env().or_else(clip_editor::ffmpeg::find_on_path)
}

fn workdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("clip-editor-test-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creating test dir");
    dir
}

/// A source shaped like the real target: two audio streams (desktop + mic),
/// a moving picture so frames are distinguishable, and a matroska container.
fn make_source(tools: &Tools, dir: &Path) -> PathBuf {
    let path = dir.join("source.mkv");
    let status = Command::new(&tools.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-t",
            "6",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
        ])
        .arg(&path)
        .status()
        .expect("running ffmpeg");
    assert!(status.success(), "failed to build the test source");
    path
}

fn make_music(tools: &Tools, dir: &Path) -> PathBuf {
    let path = dir.join("music.wav");
    let status = Command::new(&tools.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=220:sample_rate=48000",
            "-t",
            "10",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&path)
        .status()
        .expect("running ffmpeg");
    assert!(status.success());
    path
}

struct Probed {
    width: u32,
    height: u32,
    duration: f64,
    audio_streams: usize,
    video_codec: String,
    audio_codec: String,
}

fn probe(tools: &Tools, path: &Path) -> Probed {
    let json = tools
        .probe(&command::ffprobe_args(path))
        .expect("probing output");
    let v: serde_json::Value = serde_json::from_str(&json).expect("probe json");
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let video = streams.iter().find(|s| s["codec_type"] == "video");
    let audio: Vec<_> = streams
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .collect();
    Probed {
        width: video.and_then(|s| s["width"].as_u64()).unwrap_or(0) as u32,
        height: video.and_then(|s| s["height"].as_u64()).unwrap_or(0) as u32,
        duration: v["format"]["duration"]
            .as_str()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0.0),
        audio_streams: audio.len(),
        video_codec: video
            .and_then(|s| s["codec_name"].as_str())
            .unwrap_or("")
            .to_string(),
        audio_codec: audio
            .first()
            .and_then(|s| s["codec_name"].as_str())
            .unwrap_or("")
            .to_string(),
    }
}

/// Run an export to completion, returning the final result.
fn run_export(tools: &Tools, project: &Project, out: &Path) -> Result<PathBuf, String> {
    let (tx, rx) = jobs::channel();
    let cancel = Cancel::new();
    export::run_export(
        tools.clone(),
        project.clone(),
        None,
        out.to_path_buf(),
        tx,
        cancel,
    );
    let mut last_fraction = 0.0f32;
    let mut result = Err("export never reported a result".to_string());
    while let Ok(msg) = rx.try_recv() {
        match msg {
            Msg::ExportProgress { fraction, .. } => {
                assert!(
                    fraction >= last_fraction - 0.01,
                    "progress went backwards: {last_fraction} -> {fraction}"
                );
                last_fraction = fraction;
            }
            Msg::ExportDone(r) => result = r,
            _ => {}
        }
    }
    result
}

#[test]
fn probes_a_two_stream_source() {
    let Some(tools) = tools() else { return };
    let dir = workdir("probe");
    let src = make_source(&tools, &dir);

    let info = import::probe_source(&tools, &src).expect("probe");
    assert_eq!((info.width, info.height), (640, 360));
    assert_eq!(info.audio_streams.len(), 2, "desktop + mic");
    assert!(!info.is_hdr);
    assert_eq!(info.rotation, 0);
    assert!((info.duration.as_f64() - 6.0).abs() < 0.2);
    // Two audio streams must default to mixing both, or one of them silently
    // vanishes from every export.
    let project = Project::new(info);
    assert_eq!(project.original_audio.source, AudioSourceSelection::MixAll);
    assert_eq!(project.project_fps, Rational::from_int(30));
}

#[test]
fn exports_two_segments_with_a_crop() {
    let Some(tools) = tools() else { return };
    let dir = workdir("export");
    let src = make_source(&tools, &dir);
    let info = import::probe_source(&tools, &src).expect("probe");

    let mut project = Project::new(info);
    // Keep 0-1s and 3-4s: two segments, exactly two seconds of output.
    project.segments = vec![
        Segment {
            start: FrameIdx(0),
            end: FrameIdx(30),
        },
        Segment {
            start: FrameIdx(90),
            end: FrameIdx(120),
        },
    ];
    project.crop = Some(CropRect {
        x: 0,
        y: 0,
        w: 360,
        h: 360,
    });
    project.output.long_edge = Some(240);

    let out = dir.join("out.mp4");
    let path = run_export(&tools, &project, &out).expect("export should succeed");
    assert!(path.exists());

    let info = probe(&tools, &path);
    assert_eq!(
        (info.width, info.height),
        (240, 240),
        "square crop at 240px"
    );
    assert_eq!(info.video_codec, "h264");
    assert_eq!(info.audio_codec, "aac");
    assert_eq!(info.audio_streams, 1, "both source streams mixed into one");
    assert!(
        (info.duration - 2.0).abs() < 0.15,
        "expected 2s of output, got {}",
        info.duration
    );
}

#[test]
fn exports_with_an_added_music_track() {
    let Some(tools) = tools() else { return };
    let dir = workdir("music");
    let src = make_source(&tools, &dir);
    let music = make_music(&tools, &dir);
    let info = import::probe_source(&tools, &src).expect("probe");

    let mut project = Project::new(info);
    project.segments = vec![Segment {
        start: FrameIdx(0),
        end: FrameIdx(90),
    }];
    project.original_audio.gain_db = -6.0;

    let mut track = AudioTrack::new(music.clone());
    track.offset = RationalTime::new(1, 1);
    track.gain_db = -3.0;
    track.fade_out = true;
    let mut negative = AudioTrack::new(music);
    // Starts before the clip: handled by seeking into the file, not adelay.
    negative.offset = RationalTime::new(-2, 1);
    negative.gain_db = -12.0;
    project.added_tracks = vec![track, negative];

    let out = dir.join("music.mp4");
    let path = run_export(&tools, &project, &out).expect("export should succeed");
    let info = probe(&tools, &path);
    assert_eq!(info.audio_streams, 1);
    assert!(
        (info.duration - 3.0).abs() < 0.15,
        "a 10s music track must not extend a 3s clip, got {}",
        info.duration
    );
}

#[test]
fn exports_every_preset() {
    let Some(tools) = tools() else { return };
    let dir = workdir("presets");
    let src = make_source(&tools, &dir);
    let info = import::probe_source(&tools, &src).expect("probe");

    for preset in OutputPreset::ALL {
        let mut project = Project::new(info.clone());
        project.segments = vec![Segment {
            start: FrameIdx(0),
            end: FrameIdx(30),
        }];
        project.output.preset = *preset;
        // Keep the slow codecs honest about time without changing code paths.
        project.output.long_edge = Some(160);
        let out = dir.join(format!("out.{}", preset.extension()));
        match run_export(&tools, &project, &out) {
            Ok(path) => {
                let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                assert!(len > 0, "{:?} produced an empty file", preset);
            }
            Err(e) => panic!("{:?} failed to export: {e}", preset),
        }
    }
}

#[test]
fn proxy_and_frame_extraction_round_trip() {
    let Some(tools) = tools() else { return };
    let dir = workdir("proxy");
    let src = make_source(&tools, &dir);
    let info = import::probe_source(&tools, &src).expect("probe");
    let project = Project::new(info);

    let proxy = dir.join("proxy.mkv");
    let cancel = Cancel::new();
    tools
        .run_with_progress(&command::proxy_args(&project, &proxy), &cancel, |_| {})
        .expect("proxy generation");
    assert!(proxy.exists());

    // The proxy is all-intra, so a seek anywhere costs the same.
    for frame in [0u64, 45, 150] {
        let t = FrameIdx(frame).to_time(project.project_fps);
        let bytes = tools
            .run_capture_stdout(&command::proxy_frame_args(&proxy, t), &cancel)
            .expect("frame extraction");
        let width = clip_core::model::even_round(640.0 * 480.0 / 360.0) as usize;
        let expected = width * 480 * 4;
        assert_eq!(
            bytes.len(),
            expected,
            "frame {frame} decoded to the wrong size"
        );
    }
}

#[test]
fn cancelling_an_export_leaves_no_output() {
    let Some(tools) = tools() else { return };
    let dir = workdir("cancel");
    let src = make_source(&tools, &dir);
    let info = import::probe_source(&tools, &src).expect("probe");
    let mut project = Project::new(info);
    project.segments = vec![Segment {
        start: FrameIdx(0),
        end: FrameIdx(180),
    }];

    let (tx, rx) = jobs::channel();
    let cancel = Cancel::new();
    // Cancelled before it starts: the job must not produce a file.
    cancel.cancel();
    let out = dir.join("cancelled.mp4");
    export::run_export(tools.clone(), project, None, out.clone(), tx, cancel);
    let mut got_error = false;
    while let Ok(msg) = rx.try_recv() {
        if let Msg::ExportDone(Err(_)) = msg {
            got_error = true;
        }
    }
    assert!(got_error, "a cancelled export must report failure");
    assert!(!out.exists(), "a cancelled export must not leave a file");
}
