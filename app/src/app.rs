//! Application state and lifecycle. The UI thread never blocks: every long
//! operation is a worker with a cancel token, and this file only ever pushes
//! requests and drains messages.

use crate::audio::{Pcm, Waveform};
use crate::cache::SourceCache;
use crate::export;
use crate::ffmpeg::{Cancel, Tools};
use crate::import::{self, FrameRequest, FrameServer};
use crate::jobs::{self, Msg, Rx, Tx};
use crate::playback::{MixState, Player, VideoStream};
use crate::undo::UndoStack;
use clip_core::command::{self, HwEncoder, PCM_SAMPLE_RATE};
use clip_core::{FrameIdx, Project, Rational, Timeline, TrackId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// What the first-run screen is doing.
pub enum Boot {
    /// ffmpeg is available; the app is usable.
    Ready,
    /// Nothing found — offer download or a system copy.
    Missing,
    Downloading {
        done: u64,
        total: u64,
        cancel: Cancel,
    },
    Failed(String),
}

pub struct App {
    pub tx: Tx,
    pub rx: Rx,
    pub tools: Option<Tools>,
    pub hw_encoders: Vec<HwEncoder>,
    pub boot: Boot,

    pub project: Option<Project>,
    pub cache: Option<SourceCache>,
    pub proxy: Option<PathBuf>,
    pub proxy_height: u32,
    pub import_job: Option<jobs::Job>,
    pub import_progress: f32,

    pub waveform: Option<Arc<Waveform>>,
    pub pcm: Option<Arc<Pcm>>,
    /// Decoded audio for added tracks, keyed by identity rather than position.
    ///
    /// This was a `Vec` index-aligned with `project.added_tracks`, which is the
    /// condition that made a removal misroute an in-flight decode: two
    /// sequences that must be kept in step, one of them mutated from the UI and
    /// the other from a worker. A map has no positions to keep in step, and an
    /// entry with no matching track is inert rather than wrong.
    pub track_pcm: HashMap<TrackId, Arc<Pcm>>,
    pub thumbs: Vec<PathBuf>,
    pub thumb_textures: Vec<egui::TextureHandle>,

    pub frames: Option<FrameServer>,
    pub texture: Option<egui::TextureHandle>,
    pub texture_size: (u32, u32),
    pub shown_frame: Option<FrameIdx>,
    pub pending_frame: Option<FrameIdx>,
    pub generation: u64,

    /// Playhead on the SOURCE timeline, in project-fps frames.
    pub playhead: FrameIdx,
    pub selection: Option<(FrameIdx, FrameIdx)>,
    pub crop_tool: bool,
    pub crop_drag: Option<CropDrag>,

    pub player: Player,
    pub video_stream: Option<VideoStream>,
    pub shuttle: i32,

    pub export_job: Option<jobs::Job>,
    pub export_progress: f32,
    pub export_label: String,
    pub export_result: Option<Result<PathBuf, String>>,
    pub show_export: bool,
    pub output_path: Option<PathBuf>,

    pub undo: UndoStack,
    pub status: String,
    pub errors: Vec<String>,
    pub hdr_warning: bool,
}

pub struct CropDrag {
    pub handle: clip_core::crop::Handle,
    /// Pointer offset from the rect origin, for move drags.
    pub grab: egui::Vec2,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> App {
        App::with_context(&cc.egui_ctx)
    }

    /// Construction without eframe, so the UI can be driven headlessly in
    /// tests as well as by a real window.
    pub fn with_context(ctx: &egui::Context) -> App {
        let (tx, rx) = jobs::channel();
        crate::cache::cleanup();
        configure_style(ctx);

        let tools = crate::bootstrap::locate(None);
        let boot = if tools.is_some() {
            Boot::Ready
        } else {
            Boot::Missing
        };
        let hw_encoders = tools
            .as_ref()
            .map(|t| t.available_hw_encoders())
            .unwrap_or_default();
        let frames = tools
            .as_ref()
            .map(|t| FrameServer::spawn(t.clone(), tx.clone()));

        App {
            tx,
            rx,
            tools,
            hw_encoders,
            boot,
            project: None,
            cache: None,
            proxy: None,
            proxy_height: command::PROXY_HEIGHT,
            import_job: None,
            import_progress: 0.0,
            waveform: None,
            pcm: None,
            track_pcm: HashMap::new(),
            thumbs: Vec::new(),
            thumb_textures: Vec::new(),
            frames,
            texture: None,
            texture_size: (0, 0),
            shown_frame: None,
            pending_frame: None,
            generation: 0,
            playhead: FrameIdx(0),
            selection: None,
            crop_tool: false,
            crop_drag: None,
            player: Player::new(),
            video_stream: None,
            shuttle: 0,
            export_job: None,
            export_progress: 0.0,
            export_label: String::new(),
            export_result: None,
            show_export: false,
            output_path: None,
            undo: UndoStack::default(),
            status: String::new(),
            errors: Vec::new(),
            hdr_warning: false,
        }
    }

    pub fn fps(&self) -> Rational {
        self.project
            .as_ref()
            .map(|p| p.project_fps)
            .unwrap_or(Rational::from_int(30))
    }

    pub fn timeline(&self) -> Timeline {
        Timeline::new(
            self.project
                .as_ref()
                .map(|p| p.normalised_segments())
                .unwrap_or_default(),
        )
    }

    pub fn source_frames(&self) -> u64 {
        self.project
            .as_ref()
            .map(|p| p.source_frames().0)
            .unwrap_or(0)
    }

    // -- opening -------------------------------------------------------

    pub fn open_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                "Video",
                &["mkv", "mp4", "mov", "webm", "avi", "flv", "m4v", "ts"],
            )
            .pick_file()
        else {
            return;
        };
        self.open_path(path);
    }

    pub fn open_path(&mut self, path: PathBuf) {
        let Some(tools) = self.tools.clone() else {
            self.errors.push("ffmpeg is not set up yet".into());
            return;
        };
        self.cancel_import();
        self.stop_playback();
        match import::probe_source(&tools, &path) {
            Ok(source) => {
                if source.is_hdr {
                    // Producing washed-out grey output is not acceptable; the
                    // import path tone-maps, and the user is told so.
                    self.hdr_warning = true;
                }
                let project = Project::new(source);
                self.playhead = FrameIdx(0);
                self.selection = None;
                self.waveform = None;
                self.pcm = None;
                self.track_pcm.clear();
                self.thumbs.clear();
                self.thumb_textures.clear();
                self.texture = None;
                self.shown_frame = None;
                self.proxy = None;
                self.export_result = None;
                self.undo = UndoStack::default();
                self.output_path = Some(export::suggested_output(&project));
                self.project = Some(project);
                self.start_import();
                self.request_frame(FrameIdx(0));
            }
            Err(e) => self.errors.push(e),
        }
    }

    fn start_import(&mut self) {
        let (Some(tools), Some(project)) = (self.tools.clone(), self.project.clone()) else {
            return;
        };
        let cache = match SourceCache::open(&project.source.path, project.project_fps) {
            Ok(c) => c,
            Err(e) => {
                self.errors.push(format!("cache unavailable: {e}"));
                return;
            }
        };
        self.cache = Some(cache.clone());
        self.import_progress = 0.0;
        let job = jobs::Job::new("Importing");
        let cancel = job.cancel.clone();
        self.import_job = Some(job);
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("import".into())
            .spawn(move || import::run_import(tools, project, cache, tx, cancel))
            .expect("spawning import worker");
    }

    pub fn cancel_import(&mut self) {
        if let Some(job) = self.import_job.take() {
            job.cancel.cancel();
        }
    }

    /// Changing the project fps invalidates the proxy, so the import runs
    /// again against a different cache key.
    pub fn set_project_fps(&mut self, fps: Rational) {
        let Some(project) = self.project.as_mut() else {
            return;
        };
        if project.project_fps == fps {
            return;
        }
        let old = project.project_fps;
        // Edit points are frame indices, so they have to be rescaled.
        let scale = fps.as_f64() / old.as_f64();
        for seg in &mut project.segments {
            seg.start = FrameIdx((seg.start.0 as f64 * scale).round() as u64);
            seg.end = FrameIdx((seg.end.0 as f64 * scale).round() as u64);
        }
        project.project_fps = fps;
        self.playhead = FrameIdx((self.playhead.0 as f64 * scale).round() as u64);
        self.cancel_import();
        self.proxy = None;
        self.waveform = None;
        self.pcm = None;
        self.thumbs.clear();
        self.thumb_textures.clear();
        self.start_import();
    }

    // -- editing -------------------------------------------------------

    pub fn snapshot(&mut self) {
        if let Some(p) = &self.project {
            self.undo.push(p.clone());
        }
    }

    pub fn undo(&mut self) {
        if let Some(p) = self.project.as_mut() {
            if let Some(prev) = self.undo.undo(p.clone()) {
                *p = prev;
                self.after_edit();
            }
        }
    }

    pub fn redo(&mut self) {
        if let Some(p) = self.project.as_mut() {
            if let Some(next) = self.undo.redo(p.clone()) {
                *p = next;
                self.after_edit();
            }
        }
    }

    /// Called after anything that changes the edit: keeps the playhead on a
    /// kept frame and rebuilds the mix the audio callback reads.
    pub fn after_edit(&mut self) {
        let timeline = self.timeline();
        if !timeline.is_empty() && timeline.to_output(self.playhead).is_none() {
            if let Some(out) = timeline.to_output_snapped(self.playhead) {
                if let Some(src) = timeline.to_source(out) {
                    self.seek_source(src);
                }
            }
        }
        self.refresh_mix();
    }

    pub fn refresh_mix(&mut self) {
        let Some(project) = &self.project else {
            self.player.set_state(None);
            return;
        };
        let state = MixState::build(project, self.pcm.clone(), &self.track_pcm);
        self.player.set_state(Some(Arc::new(state)));
    }

    // -- transport -----------------------------------------------------

    pub fn seek_source(&mut self, frame: FrameIdx) {
        let max = self.source_frames().saturating_sub(1);
        let frame = FrameIdx(frame.0.min(max));
        self.playhead = frame;
        let fps = self.fps();
        let timeline = self.timeline();
        if let Some(out) = timeline.to_output_snapped(frame) {
            self.player.seek_frame(out, fps);
        }
        if self.player.is_playing() {
            self.restart_video_stream();
        } else {
            self.request_frame(frame);
        }
    }

    pub fn step(&mut self, delta: i64) {
        let target = if delta < 0 {
            self.playhead.saturating_sub((-delta) as u64)
        } else {
            self.playhead.saturating_add(delta as u64)
        };
        self.pause();
        self.seek_source(target);
    }

    pub fn toggle_play(&mut self) {
        if self.player.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn play(&mut self) {
        if self.project.is_none() || self.timeline().is_empty() {
            return;
        }
        self.refresh_mix();
        self.shuttle = 1;
        self.player.play();
        self.restart_video_stream();
    }

    pub fn pause(&mut self) {
        self.shuttle = 0;
        self.player.pause();
        if let Some(s) = self.video_stream.take() {
            s.stop();
        }
        let frame = self.playhead;
        self.request_frame(frame);
    }

    pub fn stop_playback(&mut self) {
        self.pause();
        self.player.set_state(None);
    }

    fn restart_video_stream(&mut self) {
        if let Some(s) = self.video_stream.take() {
            s.stop();
        }
        let (Some(tools), Some(proxy)) = (self.tools.clone(), self.proxy.clone()) else {
            return;
        };
        let (w, h) = self.proxy_dimensions();
        if w == 0 || h == 0 {
            return;
        }
        self.video_stream = Some(VideoStream::start(
            &tools,
            proxy,
            self.playhead,
            self.fps(),
            w,
            h,
        ));
    }

    fn proxy_dimensions(&self) -> (u32, u32) {
        // The proxy is scaled to a fixed height; width follows the source
        // display aspect and must be even.
        let Some(project) = &self.project else {
            return (0, 0);
        };
        let (sw, sh) = project.source.display_size();
        if sh == 0 {
            return (0, 0);
        }
        let h = self.proxy_height;
        let w = clip_core::model::even_round(sw as f32 * h as f32 / sh as f32);
        (w, h)
    }

    // -- preview frames ------------------------------------------------

    pub fn request_frame(&mut self, frame: FrameIdx) {
        let (Some(frames), Some(project)) = (&self.frames, &self.project) else {
            return;
        };
        if self.pending_frame == Some(frame) || self.shown_frame == Some(frame) {
            return;
        }
        self.generation += 1;
        let t = frame.to_time(project.project_fps);
        let (args, height) = match &self.proxy {
            Some(proxy) => (command::proxy_frame_args(proxy, t), self.proxy_height),
            // Before the proxy exists, decode straight from the source. Slow
            // to seek, but it makes the app usable the moment a file opens.
            None => (
                command::source_frame_args(project, t, command::PROXY_HEIGHT),
                command::PROXY_HEIGHT,
            ),
        };
        self.pending_frame = Some(frame);
        frames.request(FrameRequest {
            generation: self.generation,
            frame,
            args,
            height,
        });
    }

    // -- export --------------------------------------------------------

    pub fn start_export(&mut self, ctx: &egui::Context) {
        let (Some(tools), Some(project)) = (self.tools.clone(), self.project.clone()) else {
            return;
        };
        if project.normalised_segments().is_empty() {
            self.errors
                .push("nothing to export — the timeline is empty".into());
            return;
        }
        let Some(out) = self.output_path.clone() else {
            return;
        };
        let hw = self.hw_encoders.first().copied();
        let job = jobs::Job::new("Exporting");
        let cancel = job.cancel.clone();
        self.export_job = Some(job);
        self.export_progress = 0.0;
        self.export_label = "Starting…".into();
        self.export_result = None;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("export".into())
            .spawn(move || {
                export::run_export(tools, project, hw, out, tx, cancel);
                ctx.request_repaint();
            })
            .expect("spawning export worker");
    }

    pub fn cancel_export(&mut self) {
        if let Some(job) = self.export_job.take() {
            job.cancel.cancel();
        }
    }

    // -- audio tracks --------------------------------------------------

    pub fn add_audio_track(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                "Audio",
                &["mp3", "wav", "m4a", "aac", "flac", "ogg", "opus"],
            )
            .pick_file()
        else {
            return;
        };
        self.add_audio_track_from_path(path);
    }

    /// The half of `add_audio_track` that does not need a file dialog, so the
    /// add/remove/decode interleaving can be driven in a test.
    pub fn add_audio_track_from_path(&mut self, path: PathBuf) {
        let (Some(tools), Some(project), Some(cache)) = (
            self.tools.clone(),
            self.project.as_mut(),
            self.cache.clone(),
        ) else {
            return;
        };
        self.undo.push(project.clone());
        let track = clip_core::AudioTrack::new(path.clone());
        let id = track.id;
        project.added_tracks.push(track);
        // Named by id, not position: `track_2.f32` used to be handed out again
        // to a different track as soon as one was removed, so a decode still
        // running would write over the file a newer track was reading.
        let dest = scratch_path(&cache, id);
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("track-load".into())
            .spawn(move || import::load_track(tools, id, path, dest, tx))
            .expect("spawning track loader");
    }

    /// Remove by position — the UI acts on the list it is drawing, and that is
    /// a legitimate use of an index because it is resolved immediately.
    pub fn remove_audio_track(&mut self, index: usize) {
        self.snapshot();
        let Some(p) = self.project.as_mut() else {
            return;
        };
        if index >= p.added_tracks.len() {
            return;
        }
        let removed = p.added_tracks.remove(index);
        // Dropping the decoded audio here means an undo restores the track
        // without its samples, which is what the old code did too. Keeping it
        // would mean holding on to every removed track's PCM for the session —
        // ~115 MB for five minutes of stereo — for a case the user can fix by
        // re-adding the file.
        self.track_pcm.remove(&removed.id);
        if let Some(cache) = &self.cache {
            let _ = std::fs::remove_file(scratch_path(cache, removed.id));
        }
        self.refresh_mix();
    }

    /// A decode finished. The track it was for may have moved, or be gone.
    fn track_ready(
        &mut self,
        id: TrackId,
        duration: Option<clip_core::RationalTime>,
        pcm: Option<Arc<Pcm>>,
    ) {
        let track = self
            .project
            .as_mut()
            .and_then(|p| p.added_tracks.iter_mut().find(|t| t.id == id));
        let Some(track) = track else {
            // Removed while it was decoding. Nothing to attach the result to,
            // so drop it — and take the scratch file with it, since the decode
            // has finished writing by the time this arrives and nothing else
            // will ever come looking for that name.
            if let Some(cache) = &self.cache {
                let _ = std::fs::remove_file(scratch_path(cache, id));
            }
            return;
        };
        track.duration = duration;
        match pcm {
            Some(pcm) => {
                self.track_pcm.insert(id, pcm);
            }
            // A failed decode leaves the track in place but silent, rather than
            // leaving a previous track's samples under this id.
            None => {
                self.track_pcm.remove(&id);
            }
        }
        self.refresh_mix();
    }

    // -- messages ------------------------------------------------------

    pub fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status(s) => self.status = s,
                Msg::Error(e) => self.errors.push(e),
                Msg::BootstrapProgress { done, total } => {
                    if let Boot::Downloading {
                        done: d, total: t, ..
                    } = &mut self.boot
                    {
                        *d = done;
                        *t = total;
                    }
                }
                Msg::BootstrapDone(result) => match result {
                    Ok(tools) => {
                        self.hw_encoders = tools.available_hw_encoders();
                        self.frames = Some(FrameServer::spawn(tools.clone(), self.tx.clone()));
                        self.tools = Some(tools);
                        self.boot = Boot::Ready;
                    }
                    Err(e) => self.boot = Boot::Failed(e),
                },
                Msg::ProxyProgress(f) => self.import_progress = f,
                Msg::ProxyReady(path) => {
                    self.proxy = Some(path);
                    // Re-fetch the current frame from the proxy: it is sharper
                    // than the pre-proxy direct decode and, more importantly,
                    // it is the same source every later frame comes from.
                    let frame = self.playhead;
                    self.shown_frame = None;
                    self.pending_frame = None;
                    self.request_frame(frame);
                }
                Msg::PcmReady(pcm) => {
                    self.pcm = Some(pcm);
                    self.refresh_mix();
                }
                Msg::WaveformReady(w) => self.waveform = Some(w),
                Msg::ThumbsReady(t) => {
                    self.thumbs = t;
                    self.thumb_textures.clear();
                }
                Msg::ImportFailed(e) => {
                    self.errors.push(e);
                    self.import_job = None;
                }
                Msg::TrackReady { id, duration, pcm } => self.track_ready(id, duration, pcm),
                Msg::Frame(frame) => {
                    if frame.generation >= self.generation {
                        self.upload_frame(ctx, &frame);
                        self.shown_frame = Some(frame.frame);
                        self.pending_frame = None;
                    }
                }
                Msg::FrameFailed(e) => {
                    self.pending_frame = None;
                    if self.errors.len() < 8 {
                        self.errors.push(e);
                    }
                }
                Msg::ExportProgress { fraction, label } => {
                    self.export_progress = fraction;
                    self.export_label = label;
                }
                Msg::ExportDone(result) => {
                    self.export_job = None;
                    self.export_result = Some(result);
                }
            }
        }
    }

    fn upload_frame(&mut self, ctx: &egui::Context, frame: &crate::jobs::DecodedFrame) {
        let size = [frame.width as usize, frame.height as usize];
        if size[0] * size[1] * 4 > frame.rgba.len() {
            return;
        }
        let image =
            egui::ColorImage::from_rgba_unmultiplied(size, &frame.rgba[..size[0] * size[1] * 4]);
        match &mut self.texture {
            Some(tex) if self.texture_size == (frame.width, frame.height) => {
                tex.set(image, egui::TextureOptions::LINEAR);
            }
            _ => {
                self.texture =
                    Some(ctx.load_texture("preview", image, egui::TextureOptions::LINEAR));
                self.texture_size = (frame.width, frame.height);
            }
        }
    }

    /// While playing, the audio clock decides which frame is shown.
    pub fn follow_playback(&mut self, ctx: &egui::Context) {
        if !self.player.is_playing() {
            return;
        }
        let fps = self.fps();
        let timeline = self.timeline();
        let out_frame = self.player.position_frame(fps);
        let Some(src) = timeline.to_source(out_frame) else {
            // Ran off the end of the edit.
            self.pause();
            return;
        };
        // Crossing a cut means the stream is decoding the wrong part of the
        // source, so it gets restarted at the new position.
        let needs_restart = match &self.video_stream {
            Some(s) => src.0 < s.start_frame.0 || src.0 > s.start_frame.0 + 600,
            None => true,
        };
        self.playhead = src;
        if needs_restart {
            self.restart_video_stream();
        }
        let mut new_image = None;
        if let Some(stream) = &mut self.video_stream {
            if stream.advance_to(src) {
                if let Some((idx, bytes)) = &stream.current {
                    new_image = Some((*idx, stream.width, stream.height, bytes.clone()));
                }
            }
        }
        if let Some((idx, w, h, bytes)) = new_image {
            self.upload_frame(
                ctx,
                &crate::jobs::DecodedFrame {
                    generation: self.generation,
                    frame: idx,
                    width: w,
                    height: h,
                    rgba: bytes,
                },
            );
            self.shown_frame = Some(idx);
        }
        ctx.request_repaint();
    }

    pub fn playhead_seconds(&self) -> f64 {
        self.playhead.to_time(self.fps()).as_f64()
    }

    pub fn output_position_samples(&self) -> u64 {
        let timeline = self.timeline();
        timeline
            .to_output_snapped(self.playhead)
            .map(|f| (f.to_time(self.fps()).as_f64() * PCM_SAMPLE_RATE as f64) as u64)
            .unwrap_or(0)
    }
}

/// Where a track's decoded PCM is written. Named by id so the same name is
/// never handed out to two different tracks — see [`TrackId`].
fn scratch_path(cache: &SourceCache, id: TrackId) -> PathBuf {
    cache.dir.join(format!("track_{}.f32", id.0))
}

fn configure_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
    });
    // A video tool wants a dark room, whatever the system theme says.
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::from_rgb(24, 25, 28);
    visuals.window_fill = egui::Color32::from_rgb(30, 31, 35);
    visuals.extreme_bg_color = egui::Color32::from_rgb(16, 17, 19);
    ctx.set_visuals_of(egui::Theme::Dark, visuals);
}
