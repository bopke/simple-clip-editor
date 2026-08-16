//! The edit model. Everything the export pipeline needs and nothing it doesn't.

use crate::time::{FrameIdx, Rational, RationalTime};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub source: SourceInfo,
    /// Normalised timeline rate. This is the grid the UI edits and plays on
    /// and the rate the export runs at; the edit points themselves are times,
    /// so changing it re-views the edit rather than rewriting it.
    pub project_fps: Rational,
    /// Kept ranges, in output order.
    pub segments: Vec<Segment>,
    pub crop: Option<CropRect>,
    pub aspect_mode: AspectMode,
    pub original_audio: OriginalAudio,
    pub added_tracks: Vec<AudioTrack>,
    pub output: OutputSettings,
}

impl Project {
    pub fn new(source: SourceInfo) -> Project {
        let project_fps = source.default_project_fps();
        let original_audio = OriginalAudio {
            source: source.default_audio_selection(),
            gain_db: 0.0,
        };
        Project {
            // The whole source, as a duration rather than a frame count: the
            // initial segment is not an edit and should not be quantised to
            // whatever rate the project happens to open at.
            segments: vec![Segment {
                start: RationalTime::ZERO,
                end: source.duration,
            }],
            crop: None,
            aspect_mode: AspectMode::Free,
            original_audio,
            added_tracks: Vec::new(),
            output: OutputSettings::default(),
            project_fps,
            source,
        }
    }

    /// Last frame index on the timeline (exclusive end of the source).
    pub fn source_frames(&self) -> FrameIdx {
        self.source.duration_frames(self.project_fps)
    }

    /// Frames in the finished export.
    pub fn output_frames(&self) -> u64 {
        self.normalised_segments()
            .iter()
            .map(|s| s.len_frames(self.project_fps))
            .sum()
    }

    pub fn output_duration(&self) -> RationalTime {
        FrameIdx(self.output_frames()).to_time(self.project_fps)
    }

    /// Output resolution after crop/pad and the long-edge setting.
    pub fn output_size(&self) -> (u32, u32) {
        let (mut w, mut h) = match (&self.aspect_mode, &self.crop) {
            (AspectMode::PadToRatio(r), _) => pad_box(self.source.display_size(), *r),
            (_, Some(c)) => (c.w, c.h),
            (_, None) => self.source.display_size(),
        };
        if let Some(long) = self.output.long_edge {
            let (lw, lh) = scale_to_long_edge(w, h, long);
            w = lw;
            h = lh;
        }
        (even(w), even(h))
    }

    pub fn output_fps(&self) -> Rational {
        self.output.fps.unwrap_or(self.project_fps)
    }

    /// Segments sorted and with overlaps merged. Export and timeline mapping
    /// both assume this invariant, so normalise rather than trusting callers.
    pub fn normalised_segments(&self) -> Vec<Segment> {
        let mut segs: Vec<Segment> = self
            .segments
            .iter()
            .copied()
            .filter(|s| !s.is_empty())
            .collect();
        segs.sort_by_key(|s| s.start);
        let mut out: Vec<Segment> = Vec::with_capacity(segs.len());
        for s in segs {
            match out.last_mut() {
                Some(prev) if s.start <= prev.end => {
                    if s.end > prev.end {
                        prev.end = s.end;
                    }
                }
                _ => out.push(s),
            }
        }
        out
    }

    pub fn has_audio(&self) -> bool {
        !self.source.audio_streams.is_empty()
    }

    /// Mint fresh [`TrackId`]s for every added track.
    ///
    /// Ids come from a process-wide counter, so ids read from a file were
    /// minted by a different process and can collide with tracks this one
    /// already has — including scratch files named after them. A loader calls
    /// this before the project is used for anything.
    pub fn reassign_track_ids(&mut self) {
        for track in &mut self.added_tracks {
            track.id = TrackId::next();
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceInfo {
    pub path: PathBuf,
    pub duration: RationalTime,
    pub width: u32,
    pub height: u32,
    pub r_frame_rate: Rational,
    pub avg_frame_rate: Rational,
    /// From `side_data_list`; pixels may be landscape while display is portrait.
    pub rotation: i32,
    /// `color_transfer` is `smpte2084` or `arib-std-b67`.
    pub is_hdr: bool,
    pub color_range: Option<String>,
    pub colorspace: Option<String>,
    pub color_primaries: Option<String>,
    pub color_transfer: Option<String>,
    pub audio_streams: Vec<AudioStreamInfo>,
    pub video_codec: Option<String>,
    pub container: Option<String>,
}

impl SourceInfo {
    /// Dimensions after rotation metadata is applied — the orientation the
    /// crop rectangle operates in.
    pub fn display_size(&self) -> (u32, u32) {
        if self.rotation.rem_euclid(180) == 90 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    pub fn is_vfr(&self) -> bool {
        if self.r_frame_rate.is_zero() || self.avg_frame_rate.is_zero() {
            return false;
        }
        let r = self.r_frame_rate.as_f64();
        let a = self.avg_frame_rate.as_f64();
        (r - a).abs() > a * 0.02
    }

    pub fn default_project_fps(&self) -> Rational {
        let base = if !self.avg_frame_rate.is_zero() {
            self.avg_frame_rate
        } else if !self.r_frame_rate.is_zero() {
            self.r_frame_rate
        } else {
            Rational::from_int(30)
        };
        base.snap_to_common_fps()
    }

    pub fn duration_frames(&self, fps: Rational) -> FrameIdx {
        FrameIdx::from_time_round(self.duration, fps)
    }

    /// OBS records desktop audio and mic as separate streams. Defaulting to
    /// `0:a:0` silently drops one of them, so multiple streams means MixAll.
    pub fn default_audio_selection(&self) -> AudioSourceSelection {
        match self.audio_streams.len() {
            0 => AudioSourceSelection::None,
            1 => AudioSourceSelection::Stream(0),
            _ => AudioSourceSelection::MixAll,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioStreamInfo {
    /// Index among audio streams, i.e. the N in `0:a:N`.
    pub index: usize,
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub language: Option<String>,
    pub title: Option<String>,
}

impl AudioStreamInfo {
    pub fn label(&self) -> String {
        let name = self
            .title
            .clone()
            .or_else(|| self.language.clone())
            .unwrap_or_else(|| format!("Track {}", self.index + 1));
        format!("{name} ({} ch, {})", self.channels, self.codec)
    }
}

/// A kept range in source time. `start` inclusive, `end` exclusive.
///
/// Times, not frame indices: the project frame rate is a *view* of the source,
/// and changing the view must not move a cut. A boundary set at 30 fps and
/// then looked at at 23.976 is the same instant rounded onto a different grid,
/// so returning to 30 returns the original frame exactly — which rescaling
/// indices from grid to grid cannot do, since the coarser grid has fewer of
/// them than there are distinct positions to represent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: RationalTime,
    pub end: RationalTime,
}

impl Segment {
    /// A segment covering the frames `[start, end)` of the `fps` grid. The
    /// editing UI works in whole frames, so this is how an edit becomes a
    /// time: exactly, at the rate the user was looking at.
    pub fn from_frames(start: FrameIdx, end: FrameIdx, fps: Rational) -> Segment {
        Segment {
            start: start.to_time(fps),
            end: end.to_time(fps),
        }
    }

    pub fn duration(self) -> RationalTime {
        if self.is_empty() {
            RationalTime::ZERO
        } else {
            self.end - self.start
        }
    }

    pub fn is_empty(self) -> bool {
        self.end <= self.start
    }

    pub fn contains(self, t: RationalTime) -> bool {
        t >= self.start && t < self.end
    }

    /// The half-open frame range this covers on the `fps` grid. Both ends
    /// round to the nearest frame, so a segment authored at this rate comes
    /// back to the frames it was authored from.
    pub fn frame_span(self, fps: Rational) -> (FrameIdx, FrameIdx) {
        (
            FrameIdx::from_time_round(self.start, fps),
            FrameIdx::from_time_round(self.end, fps),
        )
    }

    pub fn start_frame(self, fps: Rational) -> FrameIdx {
        FrameIdx::from_time_round(self.start, fps)
    }

    pub fn end_frame(self, fps: Rational) -> FrameIdx {
        FrameIdx::from_time_round(self.end, fps)
    }

    /// Frames this contributes to the output. Derived from the rounded span
    /// rather than from the duration, so it always agrees with the frames the
    /// timeline actually maps.
    pub fn len_frames(self, fps: Rational) -> u64 {
        let (a, b) = self.frame_span(fps);
        b.0.saturating_sub(a.0)
    }

    pub fn contains_frame(self, f: FrameIdx, fps: Rational) -> bool {
        let (a, b) = self.frame_span(fps);
        f.0 >= a.0 && f.0 < b.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl CropRect {
    pub fn ratio(self) -> f32 {
        self.w as f32 / self.h.max(1) as f32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AspectMode {
    Free,
    /// Dragging any handle constrains to this ratio.
    Locked(Rational),
    /// Letterbox/pillarbox into the ratio instead of cropping to it.
    PadToRatio(Rational),
}

impl AspectMode {
    pub fn locked_ratio(self) -> Option<Rational> {
        match self {
            AspectMode::Locked(r) => Some(r),
            _ => None,
        }
    }
}

/// The presets offered in the ratio selector, in UI order.
pub const ASPECT_PRESETS: &[(&str, Rational)] = &[
    ("16:9", Rational { num: 16, den: 9 }),
    ("9:16", Rational { num: 9, den: 16 }),
    ("1:1", Rational { num: 1, den: 1 }),
    ("4:5", Rational { num: 4, den: 5 }),
    ("4:3", Rational { num: 4, den: 3 }),
    ("21:9", Rational { num: 21, den: 9 }),
];

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct OriginalAudio {
    pub source: AudioSourceSelection,
    /// 0.0 = unchanged, -inf = muted.
    pub gain_db: f32,
}

impl Default for OriginalAudio {
    fn default() -> Self {
        OriginalAudio {
            source: AudioSourceSelection::Stream(0),
            gain_db: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioSourceSelection {
    /// Mute the original entirely — it is left out of the mix, not zeroed.
    None,
    Stream(usize),
    /// `amix` every source audio stream (OBS desktop + mic).
    MixAll,
}

/// Stable identity for an added track.
///
/// A track's *position* is not identity: removing one shifts every later track
/// down, so anything that outlives the moment it was created — a background
/// decode, the name of a scratch file — has to refer to a track by something
/// that does not move, and that is not reused when a removed track's slot is
/// filled again.
///
/// Cloning a `Project` (undo snapshots) deliberately preserves ids: those are
/// the same logical tracks, so a decode in flight across an undo still lands
/// where it belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TrackId(pub u64);

impl TrackId {
    /// Ids come from a process-wide counter rather than a per-project one, so
    /// they stay unique across undo, across a project being replaced, and
    /// across two projects existing at once.
    ///
    /// Deserialised projects are the one gap: a future project loader has to
    /// reassign ids, since ids from a file were minted by a different process
    /// and can collide with this counter.
    fn next() -> TrackId {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        TrackId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioTrack {
    /// Identity, not position. See [`TrackId`].
    ///
    /// `serde(default)` so a project file written before this existed still
    /// loads; such a file needs its ids reassigned, per [`TrackId::next`].
    #[serde(default = "TrackId::next")]
    pub id: TrackId,
    pub path: PathBuf,
    /// Position on the OUTPUT timeline. May be negative, meaning the track
    /// starts before the clip does.
    pub offset: RationalTime,
    /// Optional in-point within the source file.
    pub trim_in: Option<RationalTime>,
    pub gain_db: f32,
    pub fade_out: bool,
    /// Cached duration from ffprobe, for timeline drawing. Not exported.
    #[serde(default)]
    pub duration: Option<RationalTime>,
}

impl AudioTrack {
    pub fn new(path: PathBuf) -> AudioTrack {
        AudioTrack {
            id: TrackId::next(),
            path,
            offset: RationalTime::ZERO,
            trim_in: None,
            gain_db: 0.0,
            fade_out: false,
            duration: None,
        }
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "audio".to_string())
    }

    /// A negative offset is applied by seeking into the file instead of
    /// delaying it, so the effective in-point folds the two together.
    pub fn effective_seek(&self) -> Option<RationalTime> {
        let trim = self.trim_in.unwrap_or(RationalTime::ZERO);
        let seek = if self.offset.is_negative() {
            trim + (-self.offset)
        } else {
            trim
        };
        if seek.num > 0 {
            Some(seek)
        } else {
            None
        }
    }

    /// Delay in milliseconds for `adelay`. Zero when the offset is negative
    /// (handled by seeking) or at the start.
    pub fn delay_ms(&self) -> i64 {
        if self.offset.is_negative() {
            0
        } else {
            self.offset.as_millis_round().max(0)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputSettings {
    pub preset: OutputPreset,
    /// None = native crop dimensions.
    pub long_edge: Option<u32>,
    pub quality: Quality,
    /// None = project fps.
    pub fps: Option<Rational>,
    pub use_hardware_encoder: bool,
}

impl Default for OutputSettings {
    fn default() -> Self {
        OutputSettings {
            preset: OutputPreset::Mp4H264,
            long_edge: None,
            quality: Quality::default(),
            fps: None,
            use_hardware_encoder: false,
        }
    }
}

/// Validated bundles. Container, video codec and audio codec are chosen
/// together — independent selection produces silently broken files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputPreset {
    Mp4H264,
    Mp4Hevc,
    WebmVp9,
    MovProRes,
    AudioMp3,
    AudioWav,
    Gif,
}

impl OutputPreset {
    pub const ALL: &'static [OutputPreset] = &[
        OutputPreset::Mp4H264,
        OutputPreset::Mp4Hevc,
        OutputPreset::WebmVp9,
        OutputPreset::MovProRes,
        OutputPreset::AudioMp3,
        OutputPreset::AudioWav,
        OutputPreset::Gif,
    ];

    pub fn label(self) -> &'static str {
        match self {
            OutputPreset::Mp4H264 => "MP4 / H.264",
            OutputPreset::Mp4Hevc => "MP4 / HEVC",
            OutputPreset::WebmVp9 => "WebM / VP9",
            OutputPreset::MovProRes => "MOV / ProRes",
            OutputPreset::AudioMp3 => "Audio only / MP3",
            OutputPreset::AudioWav => "Audio only / WAV",
            OutputPreset::Gif => "GIF",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            OutputPreset::Mp4H264 | OutputPreset::Mp4Hevc => "mp4",
            OutputPreset::WebmVp9 => "webm",
            OutputPreset::MovProRes => "mov",
            OutputPreset::AudioMp3 => "mp3",
            OutputPreset::AudioWav => "wav",
            OutputPreset::Gif => "gif",
        }
    }

    pub fn has_video(self) -> bool {
        !matches!(self, OutputPreset::AudioMp3 | OutputPreset::AudioWav)
    }

    pub fn has_audio(self) -> bool {
        !matches!(self, OutputPreset::Gif)
    }

    /// Whether the quality slider is a CRF for this preset.
    pub fn uses_crf(self) -> bool {
        matches!(
            self,
            OutputPreset::Mp4H264 | OutputPreset::Mp4Hevc | OutputPreset::WebmVp9
        )
    }

    /// Hardware encoders are only wired up for the H.264 path.
    pub fn supports_hardware(self) -> bool {
        matches!(self, OutputPreset::Mp4H264)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quality {
    /// Explicit CRF.
    Crf(u32),
    Bucket(QualityBucket),
}

impl Default for Quality {
    fn default() -> Self {
        Quality::Crf(18)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualityBucket {
    Small,
    Balanced,
    High,
}

impl Quality {
    /// CRF for the given preset. Codecs disagree about what a CRF means, so
    /// the bucket mapping is per-codec.
    pub fn crf_for(self, preset: OutputPreset) -> u32 {
        match self {
            Quality::Crf(v) => v,
            Quality::Bucket(b) => match (preset, b) {
                (OutputPreset::Mp4Hevc, QualityBucket::Small) => 30,
                (OutputPreset::Mp4Hevc, QualityBucket::Balanced) => 24,
                (OutputPreset::Mp4Hevc, QualityBucket::High) => 20,
                (OutputPreset::WebmVp9, QualityBucket::Small) => 36,
                (OutputPreset::WebmVp9, QualityBucket::Balanced) => 30,
                (OutputPreset::WebmVp9, QualityBucket::High) => 24,
                (_, QualityBucket::Small) => 26,
                (_, QualityBucket::Balanced) => 21,
                (_, QualityBucket::High) => 18,
            },
        }
    }
}

pub fn even(v: u32) -> u32 {
    (v & !1).max(2)
}

/// Nearest even number, matching what ffmpeg's `-2` in a `scale` filter does.
/// Truncating instead would put the app's idea of the proxy width one pixel
/// away from ffmpeg's, and every decoded frame would be misread.
pub fn even_round(v: f32) -> u32 {
    (((v / 2.0).round() as i64 * 2).max(2)) as u32
}

pub fn scale_to_long_edge(w: u32, h: u32, long: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (long, long);
    }
    if w >= h {
        let nh = (h as u64 * long as u64 + w as u64 / 2) / w as u64;
        (long, nh.max(2) as u32)
    } else {
        let nw = (w as u64 * long as u64 + h as u64 / 2) / h as u64;
        (nw.max(2) as u32, long)
    }
}

/// The smallest box of ratio `r` that contains `size` — what `pad` targets.
pub fn pad_box((w, h): (u32, u32), r: Rational) -> (u32, u32) {
    let target = r.as_f64();
    let cur = w as f64 / h.max(1) as f64;
    if cur < target {
        (even((h as f64 * target).round() as u32), even(h))
    } else {
        (even(w), even((w as f64 / target).round() as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test source runs at 60 fps; segments are named by frames of it.
    fn seg(start: u64, end: u64) -> Segment {
        Segment::from_frames(FrameIdx(start), FrameIdx(end), Rational::from_int(60))
    }

    fn source() -> SourceInfo {
        SourceInfo {
            width: 1920,
            height: 1080,
            duration: RationalTime::new(60, 1),
            avg_frame_rate: Rational::new(60, 1),
            r_frame_rate: Rational::new(60, 1),
            ..Default::default()
        }
    }

    #[test]
    fn new_project_keeps_everything() {
        let p = Project::new(source());
        assert_eq!(p.segments.len(), 1);
        assert_eq!(p.output_frames(), 3600);
        assert_eq!(p.project_fps, Rational::new(60, 1));
    }

    #[test]
    fn multiple_audio_streams_default_to_mixall() {
        let mut s = source();
        assert_eq!(s.default_audio_selection(), AudioSourceSelection::None);
        s.audio_streams.push(AudioStreamInfo {
            index: 0,
            codec: "aac".into(),
            channels: 2,
            sample_rate: 48000,
            language: None,
            title: None,
        });
        assert_eq!(s.default_audio_selection(), AudioSourceSelection::Stream(0));
        s.audio_streams.push(AudioStreamInfo {
            index: 1,
            codec: "aac".into(),
            channels: 1,
            sample_rate: 48000,
            language: None,
            title: None,
        });
        assert_eq!(s.default_audio_selection(), AudioSourceSelection::MixAll);
    }

    #[test]
    fn rotation_swaps_display_size() {
        let mut s = source();
        s.rotation = -90;
        assert_eq!(s.display_size(), (1080, 1920));
        s.rotation = 180;
        assert_eq!(s.display_size(), (1920, 1080));
    }

    #[test]
    fn vfr_detection() {
        let mut s = source();
        assert!(!s.is_vfr());
        s.r_frame_rate = Rational::new(1000, 1);
        assert!(s.is_vfr());
    }

    #[test]
    fn overlapping_segments_merge() {
        let mut p = Project::new(source());
        p.segments = vec![seg(100, 200), seg(0, 120), seg(500, 500), seg(300, 400)];
        let n = p.normalised_segments();
        assert_eq!(n, vec![seg(0, 200), seg(300, 400),]);
    }

    #[test]
    fn output_size_follows_crop_and_long_edge() {
        let mut p = Project::new(source());
        p.crop = Some(CropRect {
            x: 0,
            y: 0,
            w: 608,
            h: 1080,
        });
        assert_eq!(p.output_size(), (608, 1080));
        p.output.long_edge = Some(1920);
        // 9:16 at "1080p long edge" is the vertical case: long edge is height.
        assert_eq!(p.output_size(), (1080, 1920));
    }

    #[test]
    fn pad_mode_targets_the_ratio_box() {
        let mut p = Project::new(source());
        p.aspect_mode = AspectMode::PadToRatio(Rational::new(9, 16));
        // 1920x1080 padded to 9:16 grows vertically.
        assert_eq!(p.output_size(), (1920, 3412));
    }

    #[test]
    fn negative_offset_becomes_a_seek() {
        let mut t = AudioTrack::new("m.mp3".into());
        t.offset = RationalTime::new(-3, 1);
        assert_eq!(t.delay_ms(), 0);
        assert_eq!(t.effective_seek().unwrap().to_ffmpeg_arg(), "3.0");
        t.trim_in = Some(RationalTime::new(10, 1));
        assert_eq!(t.effective_seek().unwrap().to_ffmpeg_arg(), "13.0");
        t.offset = RationalTime::new(5, 1);
        assert_eq!(t.delay_ms(), 5000);
        assert_eq!(t.effective_seek().unwrap().to_ffmpeg_arg(), "10.0");
    }

    #[test]
    fn even_dimensions_enforced() {
        assert_eq!(even(1081), 1080);
        assert_eq!(even(0), 2);
        assert_eq!(scale_to_long_edge(1920, 1080, 1280), (1280, 720));
        assert_eq!(scale_to_long_edge(1080, 1920, 1080), (608, 1080));
        assert_eq!(even_round(853.33), 854);
        assert_eq!(even_round(852.4), 852);
        assert_eq!(even_round(0.2), 2);
    }
}
