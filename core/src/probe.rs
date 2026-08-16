//! ffprobe JSON → `SourceInfo`. Parsing only; spawning lives in the app crate.

use crate::model::{AudioStreamInfo, SourceInfo};
use crate::time::{Rational, RationalTime};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("ffprobe output was not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no video stream in {0}")]
    NoVideoStream(String),
}

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: Option<ProbeFormat>,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    format_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    #[serde(default)]
    codec_type: Option<String>,
    #[serde(default)]
    codec_name: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    r_frame_rate: Option<String>,
    #[serde(default)]
    avg_frame_rate: Option<String>,
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    color_range: Option<String>,
    #[serde(default)]
    color_space: Option<String>,
    #[serde(default)]
    color_primaries: Option<String>,
    #[serde(default)]
    color_transfer: Option<String>,
    #[serde(default)]
    channels: Option<u32>,
    #[serde(default)]
    sample_rate: Option<String>,
    #[serde(default)]
    tags: Option<StreamTags>,
    #[serde(default)]
    side_data_list: Vec<SideData>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamTags {
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    title: Option<String>,
    /// Some containers put rotation in tags rather than side data.
    #[serde(default)]
    rotate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SideData {
    #[serde(default)]
    rotation: Option<f64>,
}

/// HDR transfer functions. Naively re-encoding these to SDR H.264 gives
/// washed-out grey, so they get detected and handled rather than discovered.
pub fn is_hdr_transfer(t: &str) -> bool {
    matches!(t, "smpte2084" | "arib-std-b67")
}

pub fn parse(json: &str, path: &Path) -> Result<SourceInfo, ProbeError> {
    let out: ProbeOutput = serde_json::from_str(json)?;
    parse_output(out, path)
}

fn parse_output(out: ProbeOutput, path: &Path) -> Result<SourceInfo, ProbeError> {
    let video = out
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .ok_or_else(|| ProbeError::NoVideoStream(path.display().to_string()))?;

    let rotation = video
        .side_data_list
        .iter()
        .find_map(|s| s.rotation)
        .map(|r| r.round() as i32)
        .or_else(|| {
            video
                .tags
                .as_ref()
                .and_then(|t| t.rotate.as_ref())
                .and_then(|r| r.parse::<f64>().ok())
                .map(|r| r.round() as i32)
        })
        .unwrap_or(0);

    let duration = out
        .format
        .as_ref()
        .and_then(|f| f.duration.as_ref())
        .or(video.duration.as_ref())
        .and_then(|d| d.parse::<f64>().ok())
        .map(RationalTime::from_seconds_f64)
        .unwrap_or(RationalTime::ZERO);

    let audio_streams = out
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("audio"))
        .enumerate()
        .map(|(i, s)| AudioStreamInfo {
            index: i,
            codec: s.codec_name.clone().unwrap_or_else(|| "?".into()),
            channels: s.channels.unwrap_or(2),
            sample_rate: s
                .sample_rate
                .as_ref()
                .and_then(|r| r.parse().ok())
                .unwrap_or(48_000),
            language: s.tags.as_ref().and_then(|t| t.language.clone()),
            title: s.tags.as_ref().and_then(|t| t.title.clone()),
        })
        .collect();

    let r_frame_rate = video
        .r_frame_rate
        .as_deref()
        .and_then(Rational::parse_ffprobe)
        .unwrap_or(Rational::ZERO);
    let avg_frame_rate = video
        .avg_frame_rate
        .as_deref()
        .and_then(Rational::parse_ffprobe)
        .unwrap_or(r_frame_rate);

    let color_transfer = video.color_transfer.clone();
    Ok(SourceInfo {
        path: PathBuf::from(path),
        duration,
        width: video.width.unwrap_or(0),
        height: video.height.unwrap_or(0),
        r_frame_rate,
        avg_frame_rate,
        rotation,
        is_hdr: color_transfer.as_deref().is_some_and(is_hdr_transfer),
        color_range: video.color_range.clone(),
        colorspace: video.color_space.clone(),
        color_primaries: video.color_primaries.clone(),
        color_transfer,
        audio_streams,
        video_codec: video.codec_name.clone(),
        container: out.format.and_then(|f| f.format_name),
    })
}

/// Duration of an arbitrary media file, for added audio tracks.
pub fn parse_duration(json: &str) -> Option<RationalTime> {
    let out: ProbeOutput = serde_json::from_str(json).ok()?;
    out.format
        .and_then(|f| f.duration)
        .or_else(|| {
            let out: ProbeOutput = serde_json::from_str(json).ok()?;
            out.streams.into_iter().find_map(|s| s.duration)
        })
        .and_then(|d| d.parse::<f64>().ok())
        .map(RationalTime::from_seconds_f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like real OBS output: VFR, two audio streams, no rotation.
    const OBS: &str = r#"{
      "streams": [
        {"codec_type":"video","codec_name":"h264","width":2560,"height":1440,
         "r_frame_rate":"1000/1","avg_frame_rate":"60000/1001",
         "color_range":"tv","color_space":"bt709","color_primaries":"bt709",
         "color_transfer":"bt709"},
        {"codec_type":"audio","codec_name":"aac","channels":2,"sample_rate":"48000",
         "tags":{"title":"Desktop Audio"}},
        {"codec_type":"audio","codec_name":"aac","channels":1,"sample_rate":"48000",
         "tags":{"title":"Mic"}}
      ],
      "format": {"duration":"123.456","format_name":"matroska,webm"}
    }"#;

    #[test]
    fn parses_obs_capture() {
        let s = parse(OBS, Path::new("/tmp/clip.mkv")).unwrap();
        assert_eq!((s.width, s.height), (2560, 1440));
        assert!(s.is_vfr(), "r_frame_rate 1000 vs avg 59.94 is VFR");
        assert_eq!(s.default_project_fps(), Rational::new(60000, 1001));
        assert_eq!(s.audio_streams.len(), 2);
        assert_eq!(s.audio_streams[1].channels, 1);
        assert!(!s.is_hdr);
        assert_eq!(s.duration.to_ffmpeg_arg(), "123.456");
    }

    #[test]
    fn detects_hdr() {
        let json = OBS.replace(
            "\"color_transfer\":\"bt709\"",
            "\"color_transfer\":\"smpte2084\"",
        );
        let s = parse(&json, Path::new("x.mkv")).unwrap();
        assert!(s.is_hdr);
    }

    #[test]
    fn reads_rotation_side_data() {
        let json = r#"{"streams":[{"codec_type":"video","width":1920,"height":1080,
          "r_frame_rate":"30/1","avg_frame_rate":"30/1",
          "side_data_list":[{"rotation":-90.0}]}],"format":{"duration":"10.0"}}"#;
        let s = parse(json, Path::new("phone.mp4")).unwrap();
        assert_eq!(s.rotation, -90);
        assert_eq!(s.display_size(), (1080, 1920));
    }

    #[test]
    fn reads_rotation_tag_fallback() {
        let json = r#"{"streams":[{"codec_type":"video","width":1920,"height":1080,
          "r_frame_rate":"30/1","tags":{"rotate":"90"}}],"format":{"duration":"10.0"}}"#;
        let s = parse(json, Path::new("phone.mp4")).unwrap();
        assert_eq!(s.rotation, 90);
    }

    #[test]
    fn audio_only_file_is_rejected_as_a_source() {
        let json = r#"{"streams":[{"codec_type":"audio","codec_name":"mp3"}],"format":{}}"#;
        assert!(parse(json, Path::new("m.mp3")).is_err());
        assert!(parse_duration(r#"{"format":{"duration":"42.5"}}"#).is_some());
    }

    #[test]
    fn missing_fields_do_not_panic() {
        let json = r#"{"streams":[{"codec_type":"video"}]}"#;
        let s = parse(json, Path::new("weird.mkv")).unwrap();
        assert_eq!(s.width, 0);
        assert_eq!(s.default_project_fps(), Rational::from_int(30));
    }
}
