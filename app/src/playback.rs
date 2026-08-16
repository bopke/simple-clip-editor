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
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// One added track, resolved to samples for the mixer.
pub struct MixTrack {
    pub pcm: Arc<Pcm>,
    /// Output-timeline sample index where this track starts. Negative offsets
    /// have already been folded into `skip`.
    pub start_sample: i64,
    /// Sample offset into the file (trim in-point plus any negative offset).
    pub skip: i64,
    /// Linear, not dB. The slider is in dB and the conversion is a `powf`,
    /// which is not a thing to do 48 000 times a second per track for a value
    /// that cannot change while this state exists.
    pub gain: f32,
    pub fade_out: bool,
}

/// Everything the audio callback needs. Rebuilt whenever the edit changes.
///
/// # What is precomputed, and why
///
/// Every field below the divider is derivable from the ones above it. They are
/// stored anyway because [`sample`](Self::sample) runs once per output sample
/// and the deadline is real: at 48 kHz with a 512-frame buffer there are about
/// 10 ms to produce 512 samples, and anything that overruns is not slow, it is
/// a click. Gains were `powf` per sample per track, the fade recomputed its own
/// length per sample, and the frame-grid conversions were four `Rational`
/// divisions per sample. None of it can change without a rebuild, so none of it
/// belongs in the callback.
pub struct MixState {
    pub original: Option<Arc<Pcm>>,
    pub original_muted: bool,
    pub tracks: Vec<MixTrack>,
    pub timeline: Timeline,
    pub fps: Rational,
    /// Output-timeline length in samples.
    pub total_samples: u64,

    // -- derived ---------------------------------------------------------
    /// Linear gain for the source audio.
    original_gain: f32,
    /// Output samples per timeline frame, and its inverse. Multiplication in
    /// the callback rather than `Rational::as_f64()` divisions.
    samples_per_frame: f64,
    frames_per_sample: f64,
    /// Where the fade-out ramp starts, and how long it is.
    fade_start: u64,
    fade_samples: f32,
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
                    gain: audio_math::db_to_linear(t.gain_db),
                    fade_out: t.fade_out,
                })
            })
            .collect();
        let fade_samples = secs_to_samples(audio_math::FADE_OUT_SECS).max(1) as u64;
        let samples_per_frame = PCM_SAMPLE_RATE as f64 / fps.as_f64();
        MixState {
            original_muted: matches!(
                project.original_audio.source,
                clip_core::AudioSourceSelection::None
            ) || audio_math::is_muted(project.original_audio.gain_db),
            original,
            original_gain: audio_math::db_to_linear(project.original_audio.gain_db),
            tracks,
            timeline,
            fps,
            total_samples,
            samples_per_frame,
            frames_per_sample: 1.0 / samples_per_frame,
            fade_start: total_samples.saturating_sub(fade_samples),
            fade_samples: fade_samples as f32,
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
                let out_frame = FrameIdx((n as f64 * self.frames_per_sample) as u64);
                if let Some(src_frame) = self.timeline.to_source(out_frame) {
                    let within = n as i64 - (out_frame.0 as f64 * self.samples_per_frame) as i64;
                    let src_sample = (src_frame.0 as f64 * self.samples_per_frame) as i64 + within;
                    let (sl, sr) = pcm.frame(src_sample as isize);
                    l += sl * self.original_gain;
                    r += sr * self.original_gain;
                }
            }
        }
        for t in &self.tracks {
            let idx = n as i64 - t.start_sample + t.skip;
            if idx < t.skip {
                continue;
            }
            let mut g = t.gain;
            if t.fade_out && n >= self.fade_start && self.total_samples > 0 {
                let f = 1.0 - (n - self.fade_start) as f32 / self.fade_samples;
                g *= f.clamp(0.0, 1.0);
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

/// How many retired mix states can be queued for the UI thread to drop.
///
/// Deep enough that the callback never has to free one itself in practice;
/// bounded so that `try_send` cannot allocate. See [`Player::set_state`].
const RETIRED_DEPTH: usize = 8;

/// The audio-master clock plus the cpal stream feeding it.
pub struct Player {
    _stream: Option<cpal::Stream>,
    shared: Arc<Shared>,
    /// New mix states, handed to the callback without a lock.
    state_tx: Sender<Option<Arc<MixState>>>,
    /// Mix states the callback has finished with, to be dropped here.
    retired_rx: Receiver<Arc<MixState>>,
    /// Fallback clock for sources with no audio at all — there is nothing for
    /// audio to be master of in that case.
    silent_start: Option<std::time::Instant>,
    silent_origin: u64,
}

struct Shared {
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
            position: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            device_rate: AtomicU64::new(PCM_SAMPLE_RATE as u64),
        });
        // Unbounded towards the callback so publishing a mix can never fail and
        // leave the mixer stale; nothing is ever sent unless a stream exists to
        // drain it, so it cannot grow.
        let (state_tx, state_rx) = unbounded::<Option<Arc<MixState>>>();
        let (retired_tx, retired_rx) = bounded::<Arc<MixState>>(RETIRED_DEPTH);
        let stream = build_stream(shared.clone(), state_rx, retired_tx);
        Player {
            _stream: stream,
            shared,
            state_tx,
            retired_rx,
            silent_start: None,
            silent_origin: 0,
        }
    }

    pub fn has_audio_device(&self) -> bool {
        self._stream.is_some()
    }

    /// Publish a new mix for the callback to use.
    ///
    /// A channel rather than a mutex. The callback used to `try_lock` and, on
    /// failure, emit **a whole buffer of silence** — and this is called from
    /// every edit, so moving a gain slider during playback dropped audio. Now
    /// the callback owns its state and picks up new ones when they arrive: no
    /// lock, and no "could not get the state" branch to be wrong about.
    ///
    /// The retired states come back here to be dropped. Releasing the last
    /// `Arc<Pcm>` of a track that has just been removed frees megabytes, and
    /// `free` is not a thing to call between now and the deadline.
    pub fn set_state(&self, state: Option<Arc<MixState>>) {
        // Nothing drains the channel without a stream, so this would be an
        // unbounded queue of mixes each pinning its decoded audio.
        if self._stream.is_none() {
            return;
        }
        let _ = self.state_tx.send(state);
        while self.retired_rx.try_recv().is_ok() {}
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

/// Prefer a device configuration at [`PCM_SAMPLE_RATE`].
///
/// The mix is produced at 48 kHz, so a device running at 48 kHz needs no
/// resampling at all — and not resampling beats any resampler. Most hardware
/// supports it even when the default configuration is 44.1 kHz, which is the
/// single most common consumer output rate and was the case this fell down on.
fn preferred_config(device: &cpal::Device) -> Option<cpal::SupportedStreamConfig> {
    let default = device.default_output_config().ok()?;
    if default.sample_rate() == PCM_SAMPLE_RATE {
        return Some(default);
    }
    let native = device
        .supported_output_configs()
        .ok()?
        .filter(|range| range.sample_format() == cpal::SampleFormat::F32)
        .filter(|range| range.channels() == default.channels())
        .find_map(|range| range.try_with_sample_rate(PCM_SAMPLE_RATE));
    Some(native.unwrap_or(default))
}

/// Write one mixed frame out, mapped onto however many channels the device has.
///
/// Stereo-or-bust before: a mono device was written only the left channel and
/// silently lost the right, and a 5.1 device got the full-range program copied
/// into centre, LFE and both surrounds — which is how you find out what your
/// subwoofer sounds like.
#[inline]
fn write_frame(out: &mut [f32], l: f32, r: f32) {
    match out.len() {
        0 => {}
        1 => out[0] = (l + r) * 0.5,
        _ => {
            out[0] = l;
            out[1] = r;
            // Nothing is mixed for centre, LFE or surrounds, so they get
            // silence rather than a copy of the program.
            for c in out[2..].iter_mut() {
                *c = 0.0;
            }
        }
    }
}

/// One output sample at a possibly-fractional position in the 48 kHz mix.
///
/// `at` is in mix samples. On a 48 kHz device it is always a whole number and
/// this is exactly [`MixState::sample`]; anywhere else it lands between two of
/// them and they are interpolated. Nearest-neighbour — taking `sample(at as
/// u64)` and calling it done — is zero-order hold, and on the 44.1 kHz device
/// most people have that is audible aliasing on anything bright.
#[inline]
fn sample_at(state: &MixState, at: f64, interpolate: bool) -> (f32, f32) {
    let n = at as u64;
    if n >= state.total_samples {
        return (0.0, 0.0);
    }
    let (l0, r0) = state.sample(n);
    if !interpolate {
        return (l0, r0);
    }
    let frac = (at - n as f64) as f32;
    let (l1, r1) = if n + 1 < state.total_samples {
        state.sample(n + 1)
    } else {
        (0.0, 0.0)
    };
    (l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac)
}

fn build_stream(
    shared: Arc<Shared>,
    state_rx: Receiver<Option<Arc<MixState>>>,
    retired_tx: Sender<Arc<MixState>>,
) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let config = preferred_config(&device)?;
    let sample_rate = config.sample_rate() as f64;
    let channels = (config.channels() as usize).max(1);
    shared
        .device_rate
        .store(sample_rate as u64, Ordering::Relaxed);
    let cb_shared = shared.clone();
    let ratio = PCM_SAMPLE_RATE as f64 / sample_rate;
    // Exactly 1.0 on a 48 kHz device, which `preferred_config` gets us wherever
    // the hardware allows. Then there is no resampling to do and the mixer's
    // samples go out untouched.
    let resampling = ratio != 1.0;

    // Owned by the callback, so reading it is neither a lock nor a failure
    // case. Retired states go back to the UI thread to be dropped.
    let mut state: Option<Arc<MixState>> = None;
    // The playback position with its fraction intact. Truncating this once per
    // callback — which is what `pos + (i * ratio) as u64` amounted to — lost up
    // to a sample every ~10 ms, so the playhead drifted against the true clock
    // by up to ~100 samples a second. That drift is A/V sync, because this
    // clock is what decides which video frame is shown.
    let mut pos_f = 0.0f64;

    let stream = device
        .build_output_stream(
            config.config(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                while let Ok(next) = state_rx.try_recv() {
                    if let Some(old) = std::mem::replace(&mut state, next) {
                        let _ = retired_tx.try_send(old);
                    }
                }
                let playing = cb_shared.playing.load(Ordering::Relaxed);
                let state = match (&state, playing) {
                    (Some(state), true) => state,
                    _ => {
                        data.fill(0.0);
                        return;
                    }
                };

                // A seek moved the position out from under us; anything else is
                // our own write from the previous callback, fraction and all.
                let shared_pos = cb_shared.position.load(Ordering::Relaxed);
                if pos_f as u64 != shared_pos {
                    pos_f = shared_pos as f64;
                }

                let frames = data.len() / channels;
                for i in 0..frames {
                    let at = pos_f + i as f64 * ratio;
                    let (l, r) = sample_at(state, at, resampling);
                    write_frame(&mut data[i * channels..(i + 1) * channels], l, r);
                }

                pos_f += frames as f64 * ratio;
                let mut pos = pos_f as u64;
                if pos >= state.total_samples {
                    pos = state.total_samples;
                    pos_f = pos as f64;
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
            gain: 1.0,
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
            gain: 1.0,
            fade_out: false,
        });
        let (l, _) = state.sample(0);
        assert!(l <= audio_math::LIMITER_CEILING + 1e-6, "{l}");
        assert!(l > 0.5, "{l}");
    }

    /// A mono device used to be written only the left channel — the right was
    /// discarded rather than mixed in — and anything past stereo got the
    /// full-range program copied into centre, LFE and both surrounds.
    #[test]
    fn every_channel_layout_gets_something_sensible() {
        let mut mono = [0.0f32; 1];
        write_frame(&mut mono, 1.0, 0.0);
        assert_eq!(mono[0], 0.5, "mono is a downmix, not the left channel");

        let mut stereo = [0.0f32; 2];
        write_frame(&mut stereo, 0.25, 0.75);
        assert_eq!(stereo, [0.25, 0.75]);

        // 5.1: L, R, then centre, LFE and the surrounds, which nothing has
        // mixed anything for.
        let mut surround = [9.0f32; 6];
        write_frame(&mut surround, 0.25, 0.75);
        assert_eq!(surround, [0.25, 0.75, 0.0, 0.0, 0.0, 0.0]);

        // A zero-channel config is nonsense from the driver, not a panic here.
        write_frame(&mut [], 1.0, 1.0);
    }

    /// The gains, the fade ramp and the frame-grid conversions are resolved
    /// once in `build`. This pins the arithmetic that moved: a -6 dB track
    /// plays at half amplitude and the ramp still ends where export's does.
    #[test]
    fn gains_and_the_fade_are_resolved_before_the_callback_sees_them() {
        let mut project = project_with_cut();
        project.original_audio.gain_db = -6.0;
        let state = MixState::build(&project, Some(tone(48_000 * 2, 1.0)), &HashMap::new());
        assert!(
            (state.original_gain - 0.5012).abs() < 1e-3,
            "{}",
            state.original_gain
        );
        // Two segments of one second each.
        assert_eq!(state.total_samples, 96_000);
        assert_eq!(state.fade_samples, 96_000.0);
        assert_eq!(state.fade_start, 0);
        // 60 fps at 48 kHz: 800 samples per frame, and back again.
        assert_eq!(state.samples_per_frame, 800.0);
        assert!((state.frames_per_sample * 800.0 - 1.0).abs() < 1e-12);
    }

    /// The fade is a ramp to silence at the end of the timeline, and it has to
    /// land on the same numbers now that its length is precomputed.
    #[test]
    fn the_fade_still_ends_at_silence() {
        let project = project_with_cut();
        let mut state = MixState::build(&project, None, &HashMap::new());
        state.original_muted = true;
        state.tracks.push(MixTrack {
            pcm: tone(96_000, 0.8),
            start_sample: 0,
            skip: 0,
            gain: 1.0,
            fade_out: true,
        });
        // total_samples is 96 000 and the ramp is 2 s, so it covers the lot.
        let start = state.sample(0).0;
        let middle = state.sample(48_000).0;
        let end = state.sample(95_999).0;
        assert!((start - 0.8).abs() < 1e-6, "{start}");
        assert!((middle - 0.4).abs() < 0.01, "{middle}");
        assert!(end < 0.001, "{end}");
    }

    /// A ramp, so a value identifies the position it was read from and an
    /// interpolated one lands visibly between two of them.
    fn ramp_state(len: usize) -> MixState {
        let project = project_with_cut();
        let mut state = MixState::build(&project, None, &HashMap::new());
        state.original_muted = true;
        state.total_samples = len as u64;
        state.tracks.push(MixTrack {
            pcm: Arc::new(Pcm {
                samples: (0..len * 2).map(|i| (i / 2) as f32 * 0.001).collect(),
            }),
            start_sample: 0,
            skip: 0,
            gain: 1.0,
            fade_out: false,
        });
        state
    }

    #[test]
    fn a_position_between_two_samples_is_interpolated() {
        let state = ramp_state(1000);

        // On a 48 kHz device there is nothing to interpolate and the mixer's
        // samples go out untouched.
        assert_eq!(sample_at(&state, 10.0, false), state.sample(10));

        // Halfway between 10 and 11 is halfway between their values — where
        // zero-order hold would repeat sample 10 and step abruptly.
        let (l, _) = sample_at(&state, 10.5, true);
        let (a, _) = state.sample(10);
        let (b, _) = state.sample(11);
        assert!(
            (l - (a + b) / 2.0).abs() < 1e-6,
            "{l} not between {a} and {b}"
        );

        // The ends stay in range rather than reading past them.
        assert_eq!(sample_at(&state, 1000.0, true), (0.0, 0.0));
        assert_eq!(sample_at(&state, 2000.0, false), (0.0, 0.0));
        let (last, _) = sample_at(&state, 999.5, true);
        assert!(last.is_finite());
    }

    /// The playhead is the clock the video follows, so a position that loses
    /// its fraction every callback is A/V drift.
    ///
    /// 512 frames at 44.1 kHz — the most common consumer rate — is 557.28 mix
    /// samples, and the old accumulator threw the .28 away every buffer. That
    /// is about 24 samples a second, so a minute of playback ends up a good
    /// 30 ms out. (A buffer that divides evenly, like 441 frames at exactly
    /// 480 mix samples, loses nothing — which is why this needs a realistic
    /// buffer size to reproduce at all.)
    #[test]
    fn the_play_position_does_not_drift_against_the_true_clock() {
        let ratio = PCM_SAMPLE_RATE as f64 / 44_100.0;
        let buffer = 512usize;
        let callbacks = 1000; // ~11.6 s of audio

        let mut kept = 0.0f64;
        let mut truncated = 0u64;
        for _ in 0..callbacks {
            kept += buffer as f64 * ratio;
            // What this used to do: fold the fraction away every time.
            truncated += (buffer as f64 * ratio) as u64;
        }

        let exact = callbacks as f64 * buffer as f64 * ratio;
        assert!((kept - exact).abs() < 1e-6, "{kept} vs {exact}");
        let lost = exact - truncated as f64;
        assert!(
            lost > 200.0,
            "expected the old arithmetic to drift, lost {lost}"
        );
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
            gain: 1.0,
            fade_out: false,
        });
        assert_eq!(state.sample(0), (0.0, 0.0));
        assert_eq!(state.sample(24_000), (0.3, 0.3));
    }
}
