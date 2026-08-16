//! ffmpeg/ffprobe argument construction.
//!
//! Everything the app ever runs is built here, as a `Vec<String>`, so it can
//! be asserted against in tests without spawning anything. The golden tests at
//! the bottom of this file are the contract for the export pipeline.

use crate::audio_math::{self, FADE_OUT_SECS, LIMITER_CEILING};
use crate::model::{even, AspectMode, AudioSourceSelection, OutputPreset, Project, Segment};
use crate::time::{Rational, RationalTime};
use std::path::Path;

/// Height of the scrub proxy. 480 is enough to judge a cut and cheap to decode.
pub const PROXY_HEIGHT: u32 = 480;
/// Thumbnail strip cadence and width.
pub const THUMB_INTERVAL_SECS: u32 = 2;
pub const THUMB_WIDTH: u32 = 160;
/// Preview/playback audio format. f32 stereo at 48k, matching the mixer.
pub const PCM_SAMPLE_RATE: u32 = 48_000;
pub const PCM_CHANNELS: u32 = 2;

fn s(v: impl AsRef<str>) -> String {
    v.as_ref().to_string()
}

fn p(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// Options every ffmpeg invocation gets: no banner, no stdin stealing,
/// overwrite, and errors only on stderr (progress is parsed from stdout).
fn base() -> Vec<String> {
    vec![
        s("-hide_banner"),
        s("-nostdin"),
        s("-loglevel"),
        s("error"),
        s("-y"),
    ]
}

fn progress() -> Vec<String> {
    vec![s("-progress"), s("pipe:1"), s("-nostats")]
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

pub fn ffprobe_args(path: &Path) -> Vec<String> {
    vec![
        s("-v"),
        s("quiet"),
        s("-print_format"),
        s("json"),
        s("-show_streams"),
        s("-show_format"),
        s("-show_entries"),
        s("stream_side_data"),
        p(path),
    ]
}

// ---------------------------------------------------------------------------
// Shared filter fragments
// ---------------------------------------------------------------------------

/// Rotation metadata is normalised explicitly rather than relying on
/// autorotation, so the crop rectangle always operates on the same pixels the
/// UI drew it on. `-noautorotate` must accompany this on the input.
pub fn transpose_filter(rotation: i32) -> Option<String> {
    match rotation.rem_euclid(360) {
        90 => Some(s("transpose=clock")),
        180 => Some(s("transpose=clock,transpose=clock")),
        270 => Some(s("transpose=cclock")),
        _ => None,
    }
}

/// Tone-map HDR to SDR BT.709. Without this, a PQ source re-encoded to H.264
/// comes out washed-out grey.
pub fn tonemap_filter() -> String {
    s("zscale=t=linear:npl=100,tonemap=hable,zscale=t=bt709:m=bt709:r=tv,format=yuv420p")
}

fn needs_noautorotate(rotation: i32) -> bool {
    rotation.rem_euclid(360) != 0
}

// ---------------------------------------------------------------------------
// Import pipeline
// ---------------------------------------------------------------------------

/// MJPEG proxy at project fps. Every frame a keyframe: seeking anywhere is
/// instant, which is the whole reason scrubbing feels good.
pub fn proxy_args(project: &Project, out: &Path) -> Vec<String> {
    let src = &project.source;
    let mut a = base();
    if needs_noautorotate(src.rotation) {
        a.push(s("-noautorotate"));
    }
    a.extend([s("-i"), p(&src.path)]);

    let mut vf: Vec<String> = Vec::new();
    if let Some(t) = transpose_filter(src.rotation) {
        vf.push(t);
    }
    if src.is_hdr {
        vf.push(tonemap_filter());
    }
    vf.push(format!("fps={}", project.project_fps.to_ffmpeg_arg()));
    vf.push(format!("scale=-2:{PROXY_HEIGHT}"));

    a.extend([s("-vf"), vf.join(",")]);
    a.extend([
        s("-c:v"),
        s("mjpeg"),
        s("-q:v"),
        s("5"),
        s("-an"),
        s("-f"),
        s("matroska"),
    ]);
    a.extend(progress());
    a.push(p(out));
    a
}

/// Full-rate PCM for playback; peaks for the waveform are derived from it.
pub fn audio_extract_args(project: &Project, out: &Path) -> Vec<String> {
    let mut a = base();
    a.extend([s("-i"), p(&project.source.path)]);
    match project.original_audio.source {
        AudioSourceSelection::None | AudioSourceSelection::Stream(_) => {
            let idx = match project.original_audio.source {
                AudioSourceSelection::Stream(i) => i,
                _ => 0,
            };
            a.extend([s("-map"), format!("0:a:{idx}")]);
        }
        AudioSourceSelection::MixAll => {
            let n = project.source.audio_streams.len().max(1);
            if n > 1 {
                let inputs: String = (0..n).map(|i| format!("[0:a:{i}]")).collect();
                a.extend([
                    s("-filter_complex"),
                    format!("{inputs}amix=inputs={n}:normalize=0[a]"),
                    s("-map"),
                    s("[a]"),
                ]);
            } else {
                a.extend([s("-map"), s("0:a:0")]);
            }
        }
    }
    a.extend([
        s("-ac"),
        PCM_CHANNELS.to_string(),
        s("-ar"),
        PCM_SAMPLE_RATE.to_string(),
        s("-f"),
        s("f32le"),
    ]);
    a.extend(progress());
    a.push(p(out));
    a
}

/// PCM for an added music track, for the preview mixer.
pub fn track_pcm_args(path: &Path, out: &Path) -> Vec<String> {
    let mut a = base();
    a.extend([s("-i"), p(path)]);
    a.extend([
        s("-map"),
        s("0:a:0"),
        s("-ac"),
        PCM_CHANNELS.to_string(),
        s("-ar"),
        PCM_SAMPLE_RATE.to_string(),
        s("-f"),
        s("f32le"),
    ]);
    a.push(p(out));
    a
}

pub fn thumbnail_args(project: &Project, out_pattern: &Path) -> Vec<String> {
    let src = &project.source;
    let mut a = base();
    if needs_noautorotate(src.rotation) {
        a.push(s("-noautorotate"));
    }
    a.extend([s("-i"), p(&src.path)]);
    let mut vf: Vec<String> = Vec::new();
    if let Some(t) = transpose_filter(src.rotation) {
        vf.push(t);
    }
    if src.is_hdr {
        vf.push(tonemap_filter());
    }
    vf.push(format!("fps=1/{THUMB_INTERVAL_SECS}"));
    vf.push(format!("scale={THUMB_WIDTH}:-2"));
    a.extend([s("-vf"), vf.join(",")]);
    a.extend([s("-fps_mode"), s("vfr"), s("-q:v"), s("4")]);
    a.push(p(out_pattern));
    a
}

// ---------------------------------------------------------------------------
// Preview frames
// ---------------------------------------------------------------------------

/// One RGBA frame from the all-intra proxy. Position-independent and fast.
pub fn proxy_frame_args(proxy: &Path, t: RationalTime) -> Vec<String> {
    let mut a = base();
    a.extend([s("-ss"), t.to_ffmpeg_arg(), s("-i"), p(proxy)]);
    a.extend([
        s("-frames:v"),
        s("1"),
        s("-f"),
        s("rawvideo"),
        s("-pix_fmt"),
        s("rgba"),
        s("-"),
    ]);
    a
}

/// Continuous rawvideo from the proxy, starting at `t`. Playback reads this
/// into a small decode-ahead buffer instead of spawning a process per frame.
pub fn proxy_stream_args(proxy: &Path, t: RationalTime, fps: Rational) -> Vec<String> {
    let mut a = base();
    a.extend([s("-ss"), t.to_ffmpeg_arg(), s("-i"), p(proxy)]);
    a.extend([
        s("-vf"),
        format!("fps={}", fps.to_ffmpeg_arg()),
        s("-f"),
        s("rawvideo"),
        s("-pix_fmt"),
        s("rgba"),
        s("-"),
    ]);
    a
}

/// Direct frame extraction from the source, used before a proxy exists.
pub fn source_frame_args(project: &Project, t: RationalTime, height: u32) -> Vec<String> {
    let src = &project.source;
    let mut a = base();
    if needs_noautorotate(src.rotation) {
        a.push(s("-noautorotate"));
    }
    a.extend([s("-ss"), t.to_ffmpeg_arg(), s("-i"), p(&src.path)]);
    let mut vf: Vec<String> = Vec::new();
    if let Some(t) = transpose_filter(src.rotation) {
        vf.push(t);
    }
    if src.is_hdr {
        vf.push(tonemap_filter());
    }
    vf.push(format!("scale=-2:{height}"));
    a.extend([s("-vf"), vf.join(",")]);
    a.extend([
        s("-frames:v"),
        s("1"),
        s("-f"),
        s("rawvideo"),
        s("-pix_fmt"),
        s("rgba"),
        s("-"),
    ]);
    a
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwEncoder {
    Nvenc,
    VideoToolbox,
    Qsv,
    Vaapi,
}

impl HwEncoder {
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            HwEncoder::Nvenc => "h264_nvenc",
            HwEncoder::VideoToolbox => "h264_videotoolbox",
            HwEncoder::Qsv => "h264_qsv",
            HwEncoder::Vaapi => "h264_vaapi",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HwEncoder::Nvenc => "NVIDIA NVENC",
            HwEncoder::VideoToolbox => "Apple VideoToolbox",
            HwEncoder::Qsv => "Intel QuickSync",
            HwEncoder::Vaapi => "VA-API",
        }
    }

    pub const ALL: &'static [HwEncoder] = &[
        HwEncoder::Nvenc,
        HwEncoder::VideoToolbox,
        HwEncoder::Qsv,
        HwEncoder::Vaapi,
    ];

    /// Quality knob. Hardware encoders do not speak CRF, so the CRF value is
    /// mapped onto whatever each one uses.
    fn quality_args(self, crf: u32) -> Vec<String> {
        match self {
            HwEncoder::Nvenc => vec![
                s("-rc"),
                s("vbr"),
                s("-cq"),
                crf.to_string(),
                s("-b:v"),
                s("0"),
                s("-preset"),
                s("p5"),
            ],
            // videotoolbox's -q:v is 0-100, inverted relative to CRF.
            HwEncoder::VideoToolbox => {
                let q = (100i32 - (crf as i32 * 2)).clamp(1, 100);
                vec![s("-q:v"), q.to_string()]
            }
            HwEncoder::Qsv => vec![s("-global_quality"), crf.to_string()],
            HwEncoder::Vaapi => vec![s("-rc_mode"), s("CQP"), s("-qp"), crf.to_string()],
        }
    }
}

/// The video filter chain shared by every export segment. Identical for all
/// segments — the concat demuxer requires it and we control it, so it is.
pub fn video_filter_chain(project: &Project) -> String {
    let src = &project.source;
    let (out_w, out_h) = project.output_size();
    let mut vf: Vec<String> = Vec::new();

    if let Some(t) = transpose_filter(src.rotation) {
        vf.push(t);
    }
    if src.is_hdr {
        vf.push(tonemap_filter());
    }
    match (&project.aspect_mode, &project.crop) {
        (AspectMode::PadToRatio(_), _) => {
            vf.push(format!(
                "scale={out_w}:{out_h}:force_original_aspect_ratio=decrease"
            ));
            vf.push(format!("pad={out_w}:{out_h}:(ow-iw)/2:(oh-ih)/2"));
        }
        (_, Some(c)) => {
            vf.push(format!("crop={}:{}:{}:{}", c.w, c.h, c.x, c.y));
            vf.push(format!("scale={out_w}:{out_h}"));
        }
        (_, None) => vf.push(format!("scale={out_w}:{out_h}")),
    }
    // The fps filter is what makes a VFR source and the edit timeline agree.
    // If it is ever dropped from either this or the proxy, they diverge.
    vf.push(format!("fps={}", project.output_fps().to_ffmpeg_arg()));
    vf.push(format!("format={}", pix_fmt(project.output.preset)));
    vf.join(",")
}

fn pix_fmt(preset: OutputPreset) -> &'static str {
    match preset {
        OutputPreset::MovProRes => "yuv422p10le",
        _ => "yuv420p",
    }
}

/// Colour metadata passed explicitly, or levels come out crushed or washed
/// out depending on the source.
fn color_args() -> Vec<String> {
    vec![
        s("-color_range"),
        s("tv"),
        s("-colorspace"),
        s("bt709"),
        s("-color_primaries"),
        s("bt709"),
        s("-color_trc"),
        s("bt709"),
    ]
}

fn video_encoder_args(project: &Project, hw: Option<HwEncoder>) -> Vec<String> {
    let preset = project.output.preset;
    let crf = project.output.quality.crf_for(preset);
    if let (Some(hw), true) = (
        hw,
        preset.supports_hardware() && project.output.use_hardware_encoder,
    ) {
        let mut a = vec![s("-c:v"), s(hw.ffmpeg_name())];
        a.extend(hw.quality_args(crf));
        return a;
    }
    match preset {
        OutputPreset::Mp4H264 | OutputPreset::Gif => vec![
            s("-c:v"),
            s("libx264"),
            s("-crf"),
            crf.to_string(),
            s("-preset"),
            s("medium"),
        ],
        OutputPreset::Mp4Hevc => vec![
            s("-c:v"),
            s("libx265"),
            s("-crf"),
            crf.to_string(),
            s("-preset"),
            s("medium"),
        ],
        OutputPreset::WebmVp9 => vec![
            s("-c:v"),
            s("libvpx-vp9"),
            s("-crf"),
            crf.to_string(),
            s("-b:v"),
            s("0"),
            s("-row-mt"),
            s("1"),
        ],
        OutputPreset::MovProRes => {
            vec![s("-c:v"), s("prores_ks"), s("-profile:v"), s("3")]
        }
        OutputPreset::AudioMp3 | OutputPreset::AudioWav => vec![s("-vn")],
    }
}

/// Phase A: one segment, encoded independently so segments run in parallel.
/// Audio rides through as PCM — encoding it per segment would put an AAC
/// priming click at every join.
pub fn export_segment_args(
    project: &Project,
    seg: Segment,
    hw: Option<HwEncoder>,
    out: &Path,
) -> Vec<String> {
    let mut a = base();
    if needs_noautorotate(project.source.rotation) {
        a.push(s("-noautorotate"));
    }
    // -ss before -i is a fast seek to the preceding keyframe; because we are
    // re-encoding, ffmpeg then decodes and discards up to the exact timestamp,
    // so this is frame-accurate as well as fast.
    a.extend([
        s("-ss"),
        seg.start.to_ffmpeg_arg(),
        s("-to"),
        seg.end.to_ffmpeg_arg(),
        s("-i"),
        p(&project.source.path),
    ]);

    let preset = project.output.preset;
    if preset.has_video() || preset == OutputPreset::Gif {
        a.extend([s("-vf"), video_filter_chain(project)]);
        a.extend([s("-map"), s("0:v:0")]);
        a.extend(video_encoder_args(project, hw));
        a.extend(color_args());
    } else {
        a.push(s("-vn"));
    }

    match audio_maps(project) {
        Some(maps) => {
            for m in maps {
                a.extend([s("-map"), m]);
            }
            a.extend([s("-c:a"), s("pcm_s16le")]);
        }
        None => a.push(s("-an")),
    }

    a.extend(progress());
    a.extend([s("-f"), s("matroska"), p(out)]);
    a
}

/// Which source audio streams Phase A carries through.
fn audio_maps(project: &Project) -> Option<Vec<String>> {
    if !project.output.preset.has_audio() || project.source.audio_streams.is_empty() {
        return None;
    }
    match project.original_audio.source {
        // Muted originals are dropped here rather than carried at zero gain —
        // there is no reason to encode silence through the whole pipeline.
        AudioSourceSelection::None => None,
        AudioSourceSelection::Stream(i) => Some(vec![format!("0:a:{i}")]),
        AudioSourceSelection::MixAll => Some(
            (0..project.source.audio_streams.len())
                .map(|i| format!("0:a:{i}"))
                .collect(),
        ),
    }
}

/// Number of audio streams present in a Phase A intermediate.
fn intermediate_audio_streams(project: &Project) -> usize {
    audio_maps(project).map(|m| m.len()).unwrap_or(0)
}

/// The concat demuxer list file. Paths are single-quoted with `'` escaped,
/// which is the escaping the demuxer actually implements.
pub fn concat_list_contents(segments: &[std::path::PathBuf]) -> String {
    let mut out = String::new();
    for seg in segments {
        let path = seg.to_string_lossy().replace('\'', "'\\''");
        out.push_str(&format!("file '{path}'\n"));
    }
    out
}

/// Phase B: concat the segments, mix all audio once, encode audio once.
/// Video is stream-copied, so this is fast regardless of clip length.
pub fn export_final_args(project: &Project, list: &Path, out: &Path) -> Vec<String> {
    let preset = project.output.preset;
    let mut a = base();
    a.extend([s("-f"), s("concat"), s("-safe"), s("0"), s("-i"), p(list)]);

    // Added tracks become inputs 1..N. A negative offset is a seek into the
    // file rather than a delay, so it is applied here as an input option.
    for track in &project.added_tracks {
        if let Some(seek) = track.effective_seek() {
            a.extend([s("-ss"), seek.to_ffmpeg_arg()]);
        }
        a.extend([s("-i"), p(&track.path)]);
    }

    let audio = build_audio_filter(project);
    let video_filter = if preset == OutputPreset::Gif {
        Some(gif_filter(project))
    } else {
        None
    };

    let mut filters: Vec<String> = Vec::new();
    if let Some(v) = &video_filter {
        filters.push(v.clone());
    }
    if let Some(af) = &audio {
        filters.push(af.graph.clone());
    }
    if !filters.is_empty() {
        a.extend([s("-filter_complex"), filters.join(";")]);
    }

    match preset {
        OutputPreset::Gif => {
            a.extend([s("-map"), s("[v]"), s("-an")]);
        }
        OutputPreset::AudioMp3 | OutputPreset::AudioWav => {
            a.push(s("-vn"));
        }
        _ => {
            a.extend([s("-map"), s("0:v"), s("-c:v"), s("copy")]);
            if preset == OutputPreset::Mp4Hevc {
                // Without hvc1, Apple players refuse the file.
                a.extend([s("-tag:v"), s("hvc1")]);
            }
        }
    }

    if let Some(af) = &audio {
        a.extend([s("-map"), s(&af.out_label)]);
        a.extend(audio_encoder_args(preset));
    } else if preset.has_audio() {
        a.push(s("-an"));
    }

    if preset != OutputPreset::Gif {
        if preset == OutputPreset::Mp4H264 || preset == OutputPreset::Mp4Hevc {
            a.extend([s("-movflags"), s("+faststart")]);
        }
        // Exact output length regardless of how long the music tracks are.
        a.extend([s("-t"), project.output_duration().to_ffmpeg_arg()]);
    }

    a.extend(progress());
    a.push(p(out));
    a
}

fn audio_encoder_args(preset: OutputPreset) -> Vec<String> {
    match preset {
        OutputPreset::Mp4H264 | OutputPreset::Mp4Hevc => {
            vec![s("-c:a"), s("aac"), s("-b:a"), s("192k")]
        }
        OutputPreset::WebmVp9 => vec![s("-c:a"), s("libopus"), s("-b:a"), s("160k")],
        OutputPreset::MovProRes => vec![s("-c:a"), s("pcm_s16le")],
        OutputPreset::AudioMp3 => vec![s("-c:a"), s("libmp3lame"), s("-q:a"), s("2")],
        OutputPreset::AudioWav => vec![s("-c:a"), s("pcm_s16le")],
        OutputPreset::Gif => vec![],
    }
}

struct AudioFilter {
    graph: String,
    out_label: String,
}

/// Build the mix: original (optional) plus each added track, mixed with
/// `normalize=0` so the gain sliders mean what they say, then limited because
/// `normalize=0` is exactly what lets the sum clip.
fn build_audio_filter(project: &Project) -> Option<AudioFilter> {
    let preset = project.output.preset;
    if !preset.has_audio() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut labels: Vec<String> = Vec::new();

    let n_orig = intermediate_audio_streams(project);
    let orig_muted = audio_math::is_muted(project.original_audio.gain_db);
    if n_orig > 0 && !orig_muted {
        let gain = audio_math::linear_arg(project.original_audio.gain_db);
        if n_orig > 1 {
            let inputs: String = (0..n_orig).map(|i| format!("[0:a:{i}]")).collect();
            parts.push(format!("{inputs}amix=inputs={n_orig}:normalize=0[origmix]"));
            parts.push(format!("[origmix]volume={gain}[orig]"));
        } else {
            parts.push(format!("[0:a:0]volume={gain}[orig]"));
        }
        labels.push(s("[orig]"));
    }

    let out_dur = project.output_duration().as_f64();
    for (i, track) in project.added_tracks.iter().enumerate() {
        if audio_math::is_muted(track.gain_db) {
            continue;
        }
        let input = i + 1;
        let label = format!("[m{input}]");
        let mut chain: Vec<String> = Vec::new();
        let delay = track.delay_ms();
        if delay > 0 {
            // adelay wants milliseconds, one value per channel.
            chain.push(format!("adelay={delay}|{delay}"));
        }
        chain.push(format!("volume={}", audio_math::linear_arg(track.gain_db)));
        if track.fade_out {
            let st = (out_dur - FADE_OUT_SECS).max(0.0);
            chain.push(format!("afade=t=out:st={st:.3}:d={FADE_OUT_SECS:.1}"));
        }
        parts.push(format!("[{input}:a]{}{label}", chain.join(",")));
        labels.push(label);
    }

    if labels.is_empty() {
        return None;
    }

    let mixed = if labels.len() == 1 {
        labels[0].clone()
    } else {
        let inputs: String = labels.concat();
        // duration=first ties the mix to the original (which is exactly the
        // clip length); with no original there is no such anchor, so the -t on
        // the output does the trimming instead.
        let duration = if labels[0] == "[orig]" {
            "first"
        } else {
            "longest"
        };
        parts.push(format!(
            "{inputs}amix=inputs={}:normalize=0:duration={duration}:dropout_transition=0[mixed]",
            labels.len()
        ));
        s("[mixed]")
    };

    parts.push(format!("{mixed}alimiter=limit={LIMITER_CEILING:.2}[aout]"));
    Some(AudioFilter {
        graph: parts.join(";"),
        out_label: s("[aout]"),
    })
}

/// GIF needs a two-pass palette or it looks like 1998.
fn gif_filter(project: &Project) -> String {
    let (w, _) = project.output_size();
    let w = even(project.output.long_edge.unwrap_or(w.min(480)));
    let fps = project
        .output
        .fps
        .unwrap_or(Rational::from_int(15))
        .to_ffmpeg_arg();
    format!(
        "[0:v]fps={fps},scale={w}:-2:flags=lanczos,split[a][b];\
         [a]palettegen=stats_mode=diff[pal];\
         [b][pal]paletteuse=dither=bayer:bayer_scale=3[v]"
    )
}

/// `ffmpeg -encoders`, for detecting which hardware encoders exist.
pub fn list_encoders_args() -> Vec<String> {
    vec![s("-hide_banner"), s("-encoders")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioStreamInfo, AudioTrack, CropRect, Quality, SourceInfo};
    use crate::time::FrameIdx;
    use std::path::PathBuf;

    fn project() -> Project {
        let mut src = SourceInfo {
            path: PathBuf::from("/in/clip.mkv"),
            duration: RationalTime::new(120, 1),
            width: 1920,
            height: 1080,
            r_frame_rate: Rational::new(60, 1),
            avg_frame_rate: Rational::new(60, 1),
            ..Default::default()
        };
        src.audio_streams.push(AudioStreamInfo {
            index: 0,
            codec: "aac".into(),
            channels: 2,
            sample_rate: 48000,
            language: None,
            title: None,
        });
        Project::new(src)
    }

    /// Every project here runs at 60 fps, so a segment is named by the frames
    /// it covers on that grid.
    fn seg(start: u64, end: u64) -> Segment {
        Segment::from_frames(FrameIdx(start), FrameIdx(end), Rational::from_int(60))
    }

    fn joined(v: &[String]) -> String {
        v.join(" ")
    }

    #[test]
    fn golden_probe_args() {
        assert_eq!(
            joined(&ffprobe_args(Path::new("/in/clip.mkv"))),
            "-v quiet -print_format json -show_streams -show_format \
             -show_entries stream_side_data /in/clip.mkv"
        );
    }

    #[test]
    fn golden_proxy_args() {
        let p = project();
        assert_eq!(
            joined(&proxy_args(&p, Path::new("/cache/proxy.mkv"))),
            "-hide_banner -nostdin -loglevel error -y -i /in/clip.mkv \
             -vf fps=60,scale=-2:480 -c:v mjpeg -q:v 5 -an -f matroska \
             -progress pipe:1 -nostats /cache/proxy.mkv"
        );
    }

    #[test]
    fn proxy_normalises_rotation_and_disables_autorotate() {
        let mut p = project();
        p.source.rotation = 90;
        let a = joined(&proxy_args(&p, Path::new("/c/p.mkv")));
        assert!(a.contains("-noautorotate"), "{a}");
        assert!(a.contains("-vf transpose=clock,fps=60"), "{a}");
    }

    #[test]
    fn proxy_tonemaps_hdr() {
        let mut p = project();
        p.source.is_hdr = true;
        let a = joined(&proxy_args(&p, Path::new("/c/p.mkv")));
        assert!(a.contains("tonemap=hable"), "{a}");
    }

    #[test]
    fn golden_segment_args_default_preset() {
        let mut p = project();
        p.segments = vec![seg(60, 180)];
        let seg = p.segments[0];
        assert_eq!(
            joined(&export_segment_args(
                &p,
                seg,
                None,
                Path::new("/tmp/seg_0.mkv")
            )),
            "-hide_banner -nostdin -loglevel error -y -ss 1.0 -to 3.0 -i /in/clip.mkv \
             -vf scale=1920:1080,fps=60,format=yuv420p -map 0:v:0 \
             -c:v libx264 -crf 18 -preset medium \
             -color_range tv -colorspace bt709 -color_primaries bt709 -color_trc bt709 \
             -map 0:a:0 -c:a pcm_s16le -progress pipe:1 -nostats \
             -f matroska /tmp/seg_0.mkv"
        );
    }

    #[test]
    fn segment_args_include_crop_before_scale() {
        let mut p = project();
        p.crop = Some(CropRect {
            x: 100,
            y: 0,
            w: 608,
            h: 1080,
        });
        p.output.long_edge = Some(1920);
        let a = joined(&export_segment_args(
            &p,
            p.segments[0],
            None,
            Path::new("/t/s.mkv"),
        ));
        assert!(
            a.contains("-vf crop=608:1080:100:0,scale=1080:1920,fps=60,format=yuv420p"),
            "{a}"
        );
    }

    #[test]
    fn pad_mode_scales_then_pads() {
        let mut p = project();
        p.aspect_mode = AspectMode::PadToRatio(Rational::new(9, 16));
        let a = video_filter_chain(&p);
        assert!(
            a.starts_with("scale=1920:3412:force_original_aspect_ratio=decrease,pad=1920:3412:"),
            "{a}"
        );
    }

    #[test]
    fn every_segment_gets_identical_encoder_settings() {
        let mut p = project();
        p.segments = vec![seg(0, 60), seg(600, 900)];
        let a = export_segment_args(&p, p.segments[0], None, Path::new("/t/0.mkv"));
        let b = export_segment_args(&p, p.segments[1], None, Path::new("/t/1.mkv"));
        let strip = |v: Vec<String>| -> Vec<String> {
            // Only the -ss/-to values and the output path may differ.
            v.into_iter()
                .filter(|x| !x.contains(".mkv") && x.parse::<f64>().is_err())
                .collect()
        };
        assert_eq!(strip(a), strip(b));
    }

    #[test]
    fn mixall_maps_every_source_stream() {
        let mut p = project();
        p.source.audio_streams.push(AudioStreamInfo {
            index: 1,
            codec: "aac".into(),
            channels: 1,
            sample_rate: 48000,
            language: None,
            title: Some("Mic".into()),
        });
        p.original_audio.source = AudioSourceSelection::MixAll;
        let a = joined(&export_segment_args(
            &p,
            p.segments[0],
            None,
            Path::new("/t/s.mkv"),
        ));
        assert!(a.contains("-map 0:a:0 -map 0:a:1 -c:a pcm_s16le"), "{a}");
    }

    #[test]
    fn muted_original_is_dropped_not_zeroed() {
        let mut p = project();
        p.original_audio.source = AudioSourceSelection::None;
        let a = joined(&export_segment_args(
            &p,
            p.segments[0],
            None,
            Path::new("/t/s.mkv"),
        ));
        assert!(a.contains(" -an"), "{a}");
        let b = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.mp4"),
        ));
        assert!(!b.contains("[orig]"), "{b}");
        assert!(b.contains("-an"), "{b}");
    }

    #[test]
    fn golden_final_args_with_two_music_tracks() {
        let mut p = project();
        p.segments = vec![seg(0, 600)];
        let mut t1 = AudioTrack::new(PathBuf::from("/m/one.mp3"));
        t1.offset = RationalTime::new(5, 1);
        t1.gain_db = -6.0;
        let mut t2 = AudioTrack::new(PathBuf::from("/m/two.mp3"));
        t2.offset = RationalTime::new(-3, 1);
        t2.trim_in = Some(RationalTime::new(10, 1));
        t2.gain_db = 0.0;
        p.added_tracks = vec![t1, t2];

        assert_eq!(
            joined(&export_final_args(
                &p,
                Path::new("/t/list.txt"),
                Path::new("/o/out.mp4")
            )),
            "-hide_banner -nostdin -loglevel error -y -f concat -safe 0 -i /t/list.txt \
             -i /m/one.mp3 -ss 13.0 -i /m/two.mp3 \
             -filter_complex [0:a:0]volume=1.0000[orig];\
             [1:a]adelay=5000|5000,volume=0.5012[m1];\
             [2:a]volume=1.0000[m2];\
             [orig][m1][m2]amix=inputs=3:normalize=0:duration=first:dropout_transition=0[mixed];\
             [mixed]alimiter=limit=0.95[aout] \
             -map 0:v -c:v copy -map [aout] -c:a aac -b:a 192k -movflags +faststart \
             -t 10.0 -progress pipe:1 -nostats /o/out.mp4"
        );
    }

    /// The fade-out had no coverage on this side at all, which is how the same
    /// length came to be written down separately here and in the preview mixer.
    /// `st` is the fade's start, so it must be `FADE_OUT_SECS` before the end of
    /// the output — a 10 s output fading for 2 s starts at 8.
    #[test]
    fn fade_out_ends_with_the_output_and_lasts_the_shared_length() {
        let mut p = project();
        p.segments = vec![seg(0, 600)];
        let mut t = AudioTrack::new(PathBuf::from("/m/one.mp3"));
        t.fade_out = true;
        p.added_tracks = vec![t];

        let a = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.mp4"),
        ));
        assert!(a.contains("afade=t=out:st=8.000:d=2.0"), "{a}");
        // And the output really is 10 s, so `st` above is not coincidentally
        // right for the wrong reason.
        assert!(a.contains("-t 10.0"), "{a}");
    }

    /// A clip shorter than the fade must not produce a negative start time —
    /// ffmpeg rejects it and the whole export fails.
    #[test]
    fn fade_out_on_a_clip_shorter_than_the_fade_starts_at_zero() {
        let mut p = project();
        p.segments = vec![seg(0, 30)];
        let mut t = AudioTrack::new(PathBuf::from("/m/one.mp3"));
        t.fade_out = true;
        p.added_tracks = vec![t];

        let a = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.mp4"),
        ));
        assert!(a.contains("afade=t=out:st=0.000:d=2.0"), "{a}");
    }

    #[test]
    fn negative_offset_uses_input_seek_not_adelay() {
        let mut p = project();
        let mut t = AudioTrack::new(PathBuf::from("/m/x.mp3"));
        t.offset = RationalTime::new(-4, 1);
        p.added_tracks = vec![t];
        let a = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.mp4"),
        ));
        assert!(a.contains("-ss 4.0 -i /m/x.mp3"), "{a}");
        assert!(!a.contains("adelay"), "{a}");
    }

    #[test]
    fn mix_always_disables_normalisation_and_always_limits() {
        let mut p = project();
        p.added_tracks = vec![AudioTrack::new(PathBuf::from("/m/x.mp3"))];
        let a = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.mp4"),
        ));
        assert!(a.contains("normalize=0"), "amix must not normalise: {a}");
        assert!(
            a.contains("alimiter=limit=0.95"),
            "limiter is not optional: {a}"
        );
    }

    #[test]
    fn multiple_source_streams_are_mixed_in_phase_b() {
        let mut p = project();
        p.source.audio_streams.push(AudioStreamInfo {
            index: 1,
            codec: "aac".into(),
            channels: 1,
            sample_rate: 48000,
            language: None,
            title: None,
        });
        p.original_audio.source = AudioSourceSelection::MixAll;
        let a = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.mp4"),
        ));
        assert!(
            a.contains(
                "[0:a:0][0:a:1]amix=inputs=2:normalize=0[origmix];[origmix]volume=1.0000[orig]"
            ),
            "{a}"
        );
    }

    #[test]
    fn hevc_gets_the_apple_tag() {
        let mut p = project();
        p.output.preset = OutputPreset::Mp4Hevc;
        let a = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.mp4"),
        ));
        assert!(a.contains("-tag:v hvc1"), "{a}");
        let b = joined(&export_segment_args(
            &p,
            p.segments[0],
            None,
            Path::new("/t/s.mkv"),
        ));
        assert!(b.contains("-c:v libx265 -crf 18"), "{b}");
    }

    #[test]
    fn vp9_uses_crf_zero_bitrate_and_opus() {
        let mut p = project();
        p.output.preset = OutputPreset::WebmVp9;
        p.output.quality = Quality::Crf(30);
        let a = joined(&export_segment_args(
            &p,
            p.segments[0],
            None,
            Path::new("/t/s.mkv"),
        ));
        assert!(
            a.contains("-c:v libvpx-vp9 -crf 30 -b:v 0 -row-mt 1"),
            "{a}"
        );
        let b = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.webm"),
        ));
        assert!(b.contains("-c:a libopus"), "{b}");
    }

    #[test]
    fn audio_only_preset_skips_video_entirely() {
        let mut p = project();
        p.output.preset = OutputPreset::AudioMp3;
        let a = joined(&export_segment_args(
            &p,
            p.segments[0],
            None,
            Path::new("/t/s.mkv"),
        ));
        assert!(a.contains("-vn"), "{a}");
        assert!(!a.contains("libx264"), "{a}");
        let b = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.mp3"),
        ));
        assert!(b.contains("-vn"), "{b}");
        assert!(b.contains("-c:a libmp3lame"), "{b}");
    }

    #[test]
    fn gif_uses_a_two_pass_palette() {
        let mut p = project();
        p.output.preset = OutputPreset::Gif;
        p.output.long_edge = Some(480);
        let a = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.gif"),
        ));
        assert!(a.contains("palettegen=stats_mode=diff"), "{a}");
        assert!(a.contains("paletteuse=dither=bayer:bayer_scale=3"), "{a}");
        assert!(a.contains("-map [v] -an"), "{a}");
    }

    #[test]
    fn hardware_encoder_only_when_asked_and_supported() {
        let mut p = project();
        p.output.use_hardware_encoder = true;
        let a = joined(&export_segment_args(
            &p,
            p.segments[0],
            Some(HwEncoder::VideoToolbox),
            Path::new("/t/s.mkv"),
        ));
        assert!(a.contains("-c:v h264_videotoolbox"), "{a}");
        // A preset with no hardware path falls back to software silently.
        p.output.preset = OutputPreset::MovProRes;
        let b = joined(&export_segment_args(
            &p,
            p.segments[0],
            Some(HwEncoder::VideoToolbox),
            Path::new("/t/s.mkv"),
        ));
        assert!(b.contains("-c:v prores_ks"), "{b}");
    }

    #[test]
    fn concat_list_escapes_quotes() {
        let list = concat_list_contents(&[
            PathBuf::from("/tmp/seg_0.mkv"),
            PathBuf::from("/tmp/it's/seg_1.mkv"),
        ]);
        assert_eq!(
            list,
            "file '/tmp/seg_0.mkv'\nfile '/tmp/it'\\''s/seg_1.mkv'\n"
        );
    }

    #[test]
    fn output_length_is_pinned_to_the_timeline() {
        let mut p = project();
        p.segments = vec![seg(0, 60), seg(600, 660)];
        let a = joined(&export_final_args(
            &p,
            Path::new("/t/l.txt"),
            Path::new("/o/o.mp4"),
        ));
        assert!(a.contains("-t 2.0"), "{a}");
    }

    #[test]
    fn frame_extraction_is_rgba_raw() {
        let a = joined(&proxy_frame_args(
            Path::new("/c/p.mkv"),
            RationalTime::new(3, 2),
        ));
        assert!(a.contains("-ss 1.5 -i /c/p.mkv"), "{a}");
        assert!(
            a.ends_with("-frames:v 1 -f rawvideo -pix_fmt rgba -"),
            "{a}"
        );
    }
}
