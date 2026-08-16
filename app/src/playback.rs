//! Playback: audio clock is the master, video follows.
//!
//! Not a wall clock, and not video-driven — audio glitches are far more
//! perceptible than a dropped frame. The callback mixes live using the same
//! `audio_math` the export filter chain uses, so what you hear is what you get.

use crate::audio::Pcm;
use crate::ffmpeg::{Cancel, Tools};
use clip_core::audio_math;
use clip_core::command::{self, PCM_SAMPLE_RATE};
use clip_core::{FrameIdx, Project, Rational, RationalTime, Timeline, TrackId};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, Receiver};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One added track, resolved to samples for the mixer.
pub struct MixTrack {
    pub pcm: Arc<Pcm>,
    /// Output-timeline sample index where this track starts. Negative offsets
    /// have already been folded into `skip`.
    pub start_sample: i64,
    /// Sample offset into the file (trim in-point plus any negative offset).
    pub skip: i64,
    pub gain_db: f32,
    pub fade_out: bool,
}

/// Everything the audio callback needs. Rebuilt whenever the edit changes.
pub struct MixState {
    pub original: Option<Arc<Pcm>>,
    pub original_gain_db: f32,
    pub original_muted: bool,
    pub tracks: Vec<MixTrack>,
    pub timeline: Timeline,
    pub fps: Rational,
    /// Output-timeline length in samples.
    pub total_samples: u64,
}

impl MixState {
    pub fn build(
        project: &Project,
        original: Option<Arc<Pcm>>,
        track_pcm: &HashMap<TrackId, Arc<Pcm>>,
    ) -> MixState {
        let fps = project.project_fps;
        let timeline = Timeline::new(&project.normalised_segments(), fps);
        let total_samples =
            (timeline.duration(fps).as_f64() * PCM_SAMPLE_RATE as f64).max(0.0) as u64;
        let tracks = project
            .added_tracks
            .iter()
            .filter_map(|t| {
                // By id: a track whose decode has not finished (or failed) is
                // simply absent from the mix, and no track can pick up another
                // one's samples by standing where it used to.
                let pcm = track_pcm.get(&t.id).cloned()?;
                let start = if t.offset.is_negative() {
                    0
                } else {
                    secs_to_samples(t.offset.as_f64())
                };
                let skip = t
                    .effective_seek()
                    .map(|s| secs_to_samples(s.as_f64()))
                    .unwrap_or(0);
                Some(MixTrack {
                    pcm,
                    start_sample: start,
                    skip,
                    gain_db: t.gain_db,
                    fade_out: t.fade_out,
                })
            })
            .collect();
        MixState {
            original_muted: matches!(
                project.original_audio.source,
                clip_core::AudioSourceSelection::None
            ) || audio_math::is_muted(project.original_audio.gain_db),
            original,
            original_gain_db: project.original_audio.gain_db,
            tracks,
            timeline,
            fps,
            total_samples,
        }
    }

    /// Mix one output-timeline sample. Same gain maths and same limiter as the
    /// export chain; if these ever diverge the preview is lying.
    fn sample(&self, n: u64) -> (f32, f32) {
        let (mut l, mut r) = (0.0f32, 0.0f32);
        if !self.original_muted {
            if let Some(pcm) = &self.original {
                // Playback follows the edit, not the raw file: crossing a cut
                // skips the removed section, exactly as export does.
                let out_frame =
                    FrameIdx((n as f64 * self.fps.as_f64() / PCM_SAMPLE_RATE as f64) as u64);
                if let Some(src_frame) = self.timeline.to_source(out_frame) {
                    let within = n as i64
                        - (out_frame.0 as f64 * PCM_SAMPLE_RATE as f64 / self.fps.as_f64()) as i64;
                    let src_sample = (src_frame.0 as f64 * PCM_SAMPLE_RATE as f64
                        / self.fps.as_f64()) as i64
                        + within;
                    let (sl, sr) = pcm.frame(src_sample as isize);
                    let g = audio_math::db_to_linear(self.original_gain_db);
                    l += sl * g;
                    r += sr * g;
                }
            }
        }
        for t in &self.tracks {
            let idx = n as i64 - t.start_sample + t.skip;
            if idx < t.skip {
                continue;
            }
            let mut g = audio_math::db_to_linear(t.gain_db);
            if t.fade_out && self.total_samples > 0 {
                // Same number export uses; converted here because samples are
                // the mixer's unit, not core's.
                let fade = secs_to_samples(audio_math::FADE_OUT_SECS).max(1) as u64;
                let start = self.total_samples.saturating_sub(fade);
                if n >= start {
                    let f = 1.0 - (n - start) as f32 / fade as f32;
                    g *= f.clamp(0.0, 1.0);
                }
            }
            let (tl, tr) = t.pcm.frame(idx as isize);
            l += tl * g;
            r += tr * g;
        }
        // amix runs with normalize=0, which is exactly what allows the sum to
        // clip, so the limiter is not optional here either.
        (audio_math::limit_sample(l), audio_math::limit_sample(r))
    }
}

fn secs_to_samples(s: f64) -> i64 {
    (s * PCM_SAMPLE_RATE as f64).round() as i64
}

/// The audio-master clock plus the cpal stream feeding it.
pub struct Player {
    _stream: Option<cpal::Stream>,
    shared: Arc<Shared>,
    /// Fallback clock for sources with no audio at all — there is nothing for
    /// audio to be master of in that case.
    silent_start: Option<std::time::Instant>,
    silent_origin: u64,
}

struct Shared {
    state: Mutex<Option<Arc<MixState>>>,
    /// Output-timeline sample counter published by the callback.
    position: AtomicU64,
    playing: AtomicBool,
    device_rate: AtomicU64,
}

impl Default for Player {
    fn default() -> Self {
        Player::new()
    }
}

impl Player {
    pub fn new() -> Player {
        let shared = Arc::new(Shared {
            state: Mutex::new(None),
            position: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            device_rate: AtomicU64::new(PCM_SAMPLE_RATE as u64),
        });
        let stream = build_stream(shared.clone());
        Player {
            _stream: stream,
            shared,
            silent_start: None,
            silent_origin: 0,
        }
    }

    pub fn has_audio_device(&self) -> bool {
        self._stream.is_some()
    }

    pub fn set_state(&self, state: Option<Arc<MixState>>) {
        if let Ok(mut s) = self.shared.state.lock() {
            *s = state;
        }
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// Output-timeline position in samples.
    ///
    /// The audio callback is the clock. With no output device there is nothing
    /// for audio to be master of, so a wall clock stands in — the one case
    /// where that is the right answer rather than a shortcut.
    pub fn position_samples(&self) -> u64 {
        if let (Some(start), false) = (self.silent_start, self.has_audio_device()) {
            let elapsed = start.elapsed().as_secs_f64();
            return self.silent_origin + secs_to_samples(elapsed) as u64;
        }
        self.shared.position.load(Ordering::Relaxed)
    }

    pub fn position_frame(&self, fps: Rational) -> FrameIdx {
        let secs = self.position_samples() as f64 / PCM_SAMPLE_RATE as f64;
        FrameIdx::from_time(RationalTime::from_seconds_f64(secs), fps)
    }

    pub fn play(&mut self) {
        self.silent_start = Some(std::time::Instant::now());
        self.silent_origin = self.shared.position.load(Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&mut self) {
        // Freeze the fallback clock where it stopped.
        let pos = self.position_samples();
        self.shared.position.store(pos, Ordering::Relaxed);
        self.shared.playing.store(false, Ordering::Relaxed);
        self.silent_start = None;
    }

    /// Seek: the ring buffer is implicitly flushed because the callback reads
    /// straight from the shared counter.
    pub fn seek_samples(&mut self, samples: u64) {
        self.shared.position.store(samples, Ordering::Relaxed);
        self.silent_origin = samples;
        self.silent_start = self.is_playing().then(std::time::Instant::now);
    }

    pub fn seek_frame(&mut self, frame: FrameIdx, fps: Rational) {
        let secs = frame.to_time(fps).as_f64();
        self.seek_samples(secs_to_samples(secs).max(0) as u64);
    }
}

fn build_stream(shared: Arc<Shared>) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let config = device.default_output_config().ok()?;
    let sample_rate = config.sample_rate() as f64;
    let channels = config.channels() as usize;
    shared
        .device_rate
        .store(sample_rate as u64, Ordering::Relaxed);
    let cb_shared = shared.clone();
    let ratio = PCM_SAMPLE_RATE as f64 / sample_rate;

    let stream = device
        .build_output_stream(
            config.config(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let playing = cb_shared.playing.load(Ordering::Relaxed);
                // try_lock, never lock: the callback must never wait on the UI
                // thread rebuilding the mix.
                let state = match (cb_shared.state.try_lock(), playing) {
                    (Ok(guard), true) => guard.clone(),
                    _ => None,
                };
                let Some(state) = state else {
                    data.fill(0.0);
                    return;
                };
                let mut pos = cb_shared.position.load(Ordering::Relaxed);
                let frames = data.len() / channels.max(1);
                for i in 0..frames {
                    let n = pos + (i as f64 * ratio) as u64;
                    let (l, r) = if n >= state.total_samples {
                        (0.0, 0.0)
                    } else {
                        state.sample(n)
                    };
                    let base = i * channels;
                    for c in 0..channels {
                        data[base + c] = if c % 2 == 0 { l } else { r };
                    }
                }
                pos += (frames as f64 * ratio) as u64;
                if pos >= state.total_samples {
                    pos = state.total_samples;
                    cb_shared.playing.store(false, Ordering::Relaxed);
                }
                cb_shared.position.store(pos, Ordering::Relaxed);
            },
            |err| eprintln!("audio output error: {err}"),
            None,
        )
        .ok()?;
    stream.play().ok()?;
    Some(stream)
}

// ---------------------------------------------------------------------------
// Video decode-ahead
// ---------------------------------------------------------------------------

/// Sequential rawvideo from the proxy with a small decode-ahead buffer. One
/// ffmpeg process for a run of frames, restarted when playback crosses a cut.
pub struct VideoStream {
    pub start_frame: FrameIdx,
    pub width: u32,
    pub height: u32,
    rx: Receiver<Vec<u8>>,
    cancel: Cancel,
    next: u64,
    pub current: Option<(FrameIdx, Arc<Vec<u8>>)>,
}

impl VideoStream {
    pub fn start(
        tools: &Tools,
        proxy: PathBuf,
        start: FrameIdx,
        fps: Rational,
        width: u32,
        height: u32,
    ) -> VideoStream {
        let (tx, rx) = bounded::<Vec<u8>>(8);
        let cancel = Cancel::new();
        let args = command::proxy_stream_args(&proxy, start.to_time(fps), fps);
        let frame_bytes = (width * height * 4) as usize;
        let tools = tools.clone();
        let cancel2 = cancel.clone();
        std::thread::Builder::new()
            .name("video-stream".into())
            .spawn(move || {
                let _ = tools.stream_frames(&args, &cancel2, frame_bytes, |frame| {
                    // Blocks when the buffer is full, which is exactly the
                    // decode-ahead limit.
                    tx.send(frame).is_ok()
                });
            })
            .expect("spawning video stream");
        VideoStream {
            start_frame: start,
            width,
            height,
            rx,
            cancel,
            next: 0,
            current: None,
        }
    }

    /// Advance to `target`, dropping any frames in between. Returns true when
    /// the displayed frame changed.
    pub fn advance_to(&mut self, target: FrameIdx) -> bool {
        let mut changed = false;
        loop {
            let have = self.start_frame.0 + self.next;
            if self.current.is_some() && have > target.0 {
                break;
            }
            match self.rx.try_recv() {
                Ok(bytes) => {
                    let idx = FrameIdx(self.start_frame.0 + self.next);
                    self.next += 1;
                    self.current = Some((idx, Arc::new(bytes)));
                    changed = true;
                    if idx.0 >= target.0 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        changed
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clip_core::{Segment, SourceInfo};

    /// Export and preview now read one constant, so they cannot hold different
    /// numbers — a test asserting they are equal would be tautological. What is
    /// still able to drift is the unit conversion between them, so that is what
    /// this pins: the seconds the filter string carries, and the samples the
    /// mixer ramps over. Same trick as `audio_math::linear_arg_is_stable`.
    #[test]
    fn the_fade_is_the_same_length_in_both_units() {
        assert_eq!(format!("{:.1}", audio_math::FADE_OUT_SECS), "2.0");
        assert_eq!(
            secs_to_samples(audio_math::FADE_OUT_SECS),
            96_000,
            "2 s at 48 kHz"
        );
    }

    fn tone(len: usize, value: f32) -> Arc<Pcm> {
        Arc::new(Pcm {
            samples: vec![value; len * 2],
        })
    }

    /// The fixture source runs at 60 fps; segments are named by its frames.
    fn seg(start: u64, end: u64) -> Segment {
        Segment::from_frames(FrameIdx(start), FrameIdx(end), Rational::from_int(60))
    }

    fn project_with_cut() -> Project {
        let mut p = Project::new(SourceInfo {
            duration: RationalTime::new(10, 1),
            width: 1920,
            height: 1080,
            avg_frame_rate: Rational::new(60, 1),
            r_frame_rate: Rational::new(60, 1),
            audio_streams: vec![clip_core::AudioStreamInfo {
                index: 0,
                codec: "aac".into(),
                channels: 2,
                sample_rate: 48000,
                language: None,
                title: None,
            }],
            ..Default::default()
        });
        p.segments = vec![seg(0, 60), seg(300, 360)];
        p
    }

    #[test]
    fn playback_skips_the_removed_section() {
        let project = project_with_cut();
        // Each source second carries a distinct amplitude, all well below the
        // limiter knee so the value that comes out identifies where it came
        // from.
        let n = 48_000 * 10;
        let pcm = Arc::new(Pcm {
            samples: (0..n * 2).map(|i| 0.05 * (i / 2 / 48_000) as f32).collect(),
        });
        let state = MixState::build(&project, Some(pcm), &HashMap::new());
        // Start of the clip is source second 0.
        assert_eq!(state.sample(0).0, 0.0);
        // The first segment is frames 0..60 at 60 fps — one second — after
        // which playback must land at source second 5, not second 1.
        let (l, _) = state.sample(48_000);
        assert!(
            (l - 0.25).abs() < 0.01,
            "expected source second 5 (0.25), got {l}"
        );
        // Halfway through the second segment: source second 5.5.
        let (l, _) = state.sample(48_000 + 24_000);
        assert!((l - 0.25).abs() < 0.01, "got {l}");
    }

    #[test]
    fn muted_original_is_silent_but_tracks_still_play() {
        let mut project = project_with_cut();
        project.original_audio.gain_db = f32::NEG_INFINITY;
        let mut state = MixState::build(&project, Some(tone(48_000, 0.5)), &HashMap::new());
        assert_eq!(state.sample(10), (0.0, 0.0));
        state.tracks.push(MixTrack {
            pcm: tone(48_000, 0.25),
            start_sample: 0,
            skip: 0,
            gain_db: 0.0,
            fade_out: false,
        });
        assert_eq!(state.sample(10), (0.25, 0.25));
    }

    #[test]
    fn mix_is_limited_not_clipped_raw() {
        let mut project = project_with_cut();
        project.original_audio.gain_db = 0.0;
        let mut state = MixState::build(&project, Some(tone(48_000, 0.9)), &HashMap::new());
        state.tracks.push(MixTrack {
            pcm: tone(48_000, 0.9),
            start_sample: 0,
            skip: 0,
            gain_db: 0.0,
            fade_out: false,
        });
        let (l, _) = state.sample(0);
        assert!(l <= audio_math::LIMITER_CEILING + 1e-6, "{l}");
        assert!(l > 0.5, "{l}");
    }

    #[test]
    fn track_offset_delays_playback() {
        let project = project_with_cut();
        let mut state = MixState::build(&project, None, &HashMap::new());
        state.original_muted = true;
        state.tracks.push(MixTrack {
            pcm: tone(48_000, 0.3),
            start_sample: 24_000,
            skip: 0,
            gain_db: 0.0,
            fade_out: false,
        });
        assert_eq!(state.sample(0), (0.0, 0.0));
        assert_eq!(state.sample(24_000), (0.3, 0.3));
    }
}
